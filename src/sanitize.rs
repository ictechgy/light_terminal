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
    strip_controls(&terminal_capture(value.as_bytes()))
}

pub fn terminal_capture(bytes: &[u8]) -> String {
    let mut state = TerminalCaptureState::default();
    let mut out = terminal_capture_from_state(bytes, &mut state);
    state.flush_pending_utf8_at_capture_end(&mut out);
    out
}

pub(crate) fn terminal_capture_from_state(
    bytes: &[u8],
    state: &mut TerminalCaptureState,
) -> String {
    let mut pending_prefixed = Vec::new();
    let bytes = if state.pending_utf8_len != 0 {
        pending_prefixed.reserve(usize::from(state.pending_utf8_len) + bytes.len());
        pending_prefixed.extend_from_slice(state.pending_utf8());
        pending_prefixed.extend_from_slice(bytes);
        state.clear_pending_utf8();
        pending_prefixed.as_slice()
    } else {
        bytes
    };
    let mut out = String::with_capacity(bytes.len());
    let mut index = 0_usize;

    while index < bytes.len() {
        let byte = bytes[index];
        match state.escape {
            EscapeState::Ground => match byte {
                0x1b => state.escape = EscapeState::Esc,
                0x9b => state.escape = EscapeState::Csi,
                0x90 | 0x98 | 0x9d | 0x9e | 0x9f => state.escape = EscapeState::String,
                0x80..=0x9f => {}
                b'\t' | b'\n' | b'\r' => out.push(byte as char),
                0x00..=0x1f | 0x7f => {}
                0x00..=0x7f => out.push(byte as char),
                _ => {
                    if let Some((ch, len)) = decode_utf8_char(&bytes[index..]) {
                        match ch {
                            '\u{009b}' => state.escape = EscapeState::Csi,
                            '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                                state.escape = EscapeState::String
                            }
                            ch if is_c0_or_c1(ch) => {}
                            _ => out.push(ch),
                        }
                        index += len;
                        continue;
                    }
                    if state.store_incomplete_utf8(&bytes[index..]) {
                        break;
                    }
                    out.push('\u{fffd}');
                }
            },
            EscapeState::Esc => match byte {
                0x18 | 0x1a => state.escape = EscapeState::Ground,
                0x1b => state.escape = EscapeState::Esc,
                b'[' => state.escape = EscapeState::Csi,
                b']' | b'P' | b'_' | b'^' | b'X' => state.escape = EscapeState::String,
                b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => {
                    state.escape = EscapeState::Charset
                }
                0x9b => state.escape = EscapeState::Csi,
                0x90 | 0x98 | 0x9d | 0x9e | 0x9f => state.escape = EscapeState::String,
                0x20..=0x2f => {}
                _ => state.escape = EscapeState::Ground,
            },
            EscapeState::Csi => match byte {
                byte if byte >= 0x80 => {
                    if let Some((ch, len)) = decode_utf8_char(&bytes[index..]) {
                        if ch == '\u{009c}' {
                            state.escape = EscapeState::Ground;
                        }
                        index += len;
                        continue;
                    }
                    if state.store_incomplete_utf8(&bytes[index..]) {
                        break;
                    }
                    if byte == 0x9c {
                        state.escape = EscapeState::Ground;
                    }
                }
                0x18 | 0x1a | 0x9c => state.escape = EscapeState::Ground,
                0x1b => state.escape = EscapeState::Esc,
                byte if (0x40..=0x7e).contains(&byte) => state.escape = EscapeState::Ground,
                _ => {}
            },
            EscapeState::String => match byte {
                byte if byte >= 0x80 => {
                    if let Some((ch, len)) = decode_utf8_char(&bytes[index..]) {
                        if ch == '\u{009c}' {
                            state.escape = EscapeState::Ground;
                        }
                        index += len;
                        continue;
                    }
                    if state.store_incomplete_utf8(&bytes[index..]) {
                        break;
                    }
                    if byte == 0x9c {
                        state.escape = EscapeState::Ground;
                    }
                }
                0x18 | 0x1a => state.escape = EscapeState::Ground,
                0x07 | 0x9c => state.escape = EscapeState::Ground,
                0x1b => state.escape = EscapeState::StringEsc,
                _ => {}
            },
            EscapeState::StringEsc => {
                if byte >= 0x80 {
                    if let Some((ch, len)) = decode_utf8_char(&bytes[index..]) {
                        state.escape = if ch == '\u{009c}' {
                            EscapeState::Ground
                        } else {
                            EscapeState::String
                        };
                        index += len;
                        continue;
                    }
                    if state.store_incomplete_utf8(&bytes[index..]) {
                        break;
                    }
                }
                state.escape = if byte == b'\\' || byte == 0x9c {
                    EscapeState::Ground
                } else {
                    EscapeState::String
                };
            }
            EscapeState::Charset => state.escape = EscapeState::Ground,
        }
        index += 1;
    }

    out
}

/// 신뢰할 수 없는 외부 명령(understatus 등)의 stdout을 lterm 하단 status row에
/// 안전하게 그리기 위한 **단일 행** 살균 함수.
///
/// `terminal_capture`의 상태머신을 본떠 작성하되, 위험한 escape는 전부 차단하고
/// `allow_color == true`일 때 **SGR(색) 시퀀스만** 선택적으로 통과시킨다.
/// 폭 절단은 하지 않는다 — 그 책임은 `truncate_status_line_ansi`가 진다.
///
/// 파라미터:
/// - `bytes`: 신뢰 불가 stdout 원시 바이트.
/// - `allow_color`: true면 유효한 SGR(`\x1b[<params>m`)을 정규화해 통과시키고,
///   false면 색을 포함한 모든 escape를 제거한 plain 단일행을 반환한다.
///
/// 반환값: 단일 행 안전 문자열. `allow_color`이고 SGR을 하나라도 emit했다면
/// 색 누수 차단을 위해 끝에 `\x1b[0m`를 부착한다.
///
/// 주의: 첫 `\n`/`\r`에서 입력을 중단한다(이후 바이트 무시). 미완결 CSI는
/// 아무것도 emit하지 않아 payload 누수를 0으로 유지한다.
pub fn sanitize_status_command_line(bytes: &[u8], allow_color: bool) -> String {
    // 파서 폭주 방지를 위한 CSI 파라미터 한도.
    const MAX_CSI_PARAM_BYTES: usize = 64;
    // NOTE: COUNT는 `;` 구분자만 센다. truecolor 콜론형(`38:2:r:g:b`)처럼 `:`로 묶인
    // 서브파라미터는 별도로 세지 않으며, 길이는 MAX_CSI_PARAM_BYTES(64)로만 상한이 걸린다.
    // 콜론형은 항상 단일 SGR 토큰이라 개수 폭주 위험이 없어 의도적으로 동작을 바꾸지 않는다.
    const MAX_CSI_PARAM_COUNT: usize = 16;

    let mut out = String::with_capacity(bytes.len());
    let mut state = EscapeState::Ground;
    // CSI 파라미터/intermediate 바이트 누적 버퍼와 유효성 플래그.
    let mut csi_params: Vec<u8> = Vec::new();
    let mut csi_valid = true;
    let mut emitted_sgr = false;
    let mut index = 0_usize;

    while index < bytes.len() {
        let byte = bytes[index];
        // 단일 행 계약: LF/CR은 **모든 파서 상태**에서 정지 바이트로 취급한다.
        // Ground뿐 아니라 OSC/DCS/CSI/escape 도중에 줄바꿈이 들어와도 즉시 멈춰,
        // 이스케이프 내부 LF 같은 비정상 입력이 다음 줄로 누수되지 않게 한다.
        if byte == b'\n' || byte == b'\r' {
            break;
        }
        match state {
            EscapeState::Ground => match byte {
                0x1b => state = EscapeState::Esc,
                0x9b => {
                    csi_params.clear();
                    csi_valid = true;
                    state = EscapeState::Csi;
                }
                0x90 | 0x98 | 0x9d | 0x9e | 0x9f => state = EscapeState::String,
                0x80..=0x9f => {}
                b'\t' => {}
                0x00..=0x1f | 0x7f => {}
                0x00..=0x7f => out.push(byte as char),
                _ => {
                    if let Some((ch, len)) = decode_utf8_char(&bytes[index..]) {
                        match ch {
                            '\u{009b}' => {
                                csi_params.clear();
                                csi_valid = true;
                                state = EscapeState::Csi;
                            }
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
                b'[' => {
                    csi_params.clear();
                    csi_valid = true;
                    state = EscapeState::Csi;
                }
                b']' | b'P' | b'_' | b'^' | b'X' => state = EscapeState::String,
                b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => state = EscapeState::Charset,
                0x9b => {
                    csi_params.clear();
                    csi_valid = true;
                    state = EscapeState::Csi;
                }
                0x90 | 0x98 | 0x9d | 0x9e | 0x9f => state = EscapeState::String,
                0x20..=0x2f => {}
                _ => state = EscapeState::Ground,
            },
            EscapeState::Csi => match byte {
                byte if byte >= 0x80 => {
                    if let Some((ch, len)) = decode_utf8_char(&bytes[index..]) {
                        if ch == '\u{009c}' {
                            state = EscapeState::Ground;
                        } else {
                            csi_valid = false;
                        }
                        index += len;
                        continue;
                    }
                    if byte == 0x9c {
                        state = EscapeState::Ground;
                    } else {
                        csi_valid = false;
                    }
                }
                // CAN/SUB/ST: 시퀀스 중단 후 폐기.
                0x18 | 0x1a | 0x9c => state = EscapeState::Ground,
                // 중간 ESC는 재동기화.
                0x1b => state = EscapeState::Esc,
                // 최종 바이트에서 판정.
                byte if (0x40..=0x7e).contains(&byte) => {
                    let is_sgr = byte == b'm';
                    // 파라미터 개수 = (`;` 개수 + 1). count + 1 <= MAX 는 count < MAX 와 동치.
                    let within_limits = csi_params.len() <= MAX_CSI_PARAM_BYTES
                        && csi_params.iter().filter(|&&b| b == b';').count() < MAX_CSI_PARAM_COUNT;
                    if allow_color && is_sgr && csi_valid && within_limits {
                        // 정규 `\x1b[<params>m` 형태로 emit(C1형도 정규화됨).
                        out.push('\u{001b}');
                        out.push('[');
                        // 파라미터 바이트는 `[0-9;:]`만 통과했으므로 ASCII 안전.
                        for &param_byte in &csi_params {
                            out.push(param_byte as char);
                        }
                        out.push('m');
                        emitted_sgr = true;
                    }
                    // SGR이 아니거나 무효/한도 초과면 시퀀스 전체 폐기.
                    state = EscapeState::Ground;
                }
                // 파라미터 바이트: `[0-9;:]`만 유효.
                byte if matches!(byte, b'0'..=b'9' | b';' | b':') => {
                    if csi_valid {
                        csi_params.push(byte);
                    }
                }
                // private(`<=>?`)·intermediate(`0x20..=0x2f`)·기타는 무효 표시.
                _ => csi_valid = false,
            },
            EscapeState::String => match byte {
                byte if byte >= 0x80 => {
                    if let Some((ch, len)) = decode_utf8_char(&bytes[index..]) {
                        if ch == '\u{009c}' {
                            state = EscapeState::Ground;
                        }
                        index += len;
                        continue;
                    }
                    if byte == 0x9c {
                        state = EscapeState::Ground;
                    }
                }
                0x18 | 0x1a => state = EscapeState::Ground,
                0x07 | 0x9c => state = EscapeState::Ground,
                0x1b => state = EscapeState::StringEsc,
                _ => {}
            },
            EscapeState::StringEsc => {
                if byte >= 0x80 {
                    if let Some((ch, len)) = decode_utf8_char(&bytes[index..]) {
                        state = if ch == '\u{009c}' {
                            EscapeState::Ground
                        } else {
                            EscapeState::String
                        };
                        index += len;
                        continue;
                    }
                }
                state = if byte == b'\\' || byte == 0x9c {
                    EscapeState::Ground
                } else {
                    EscapeState::String
                };
            }
            EscapeState::Charset => state = EscapeState::Ground,
        }
        index += 1;
    }

    // 색 누수 차단: SGR을 하나라도 emit했으면 reset으로 닫는다.
    if emitted_sgr {
        out.push_str("\u{001b}[0m");
    }

    out
}

/// `sanitize_status_command_line` 출력(완결 SGR만 포함된 단일행)을 **ANSI-aware**로
/// 폭 절단한다. `format_status_line`의 grapheme/CJK/ellipsis 정책과 일관되게 동작한다.
///
/// 파라미터:
/// - `line`: 살균된 단일행(SGR 시퀀스 외 위험 escape 없음).
/// - `max_width`: status row의 가용 표시 폭(셀 단위).
///
/// 반환값: 폭이 `max_width`를 넘지 않는 문자열. SGR 시퀀스는 폭 0으로 통과하며
/// 절대 중간에서 잘리지 않는다(원자적 취급). 출력에 SGR이 하나라도 있으면 색 누수
/// 차단을 위해 끝에 `\x1b[0m`를 부착한다.
///
/// 주의: 절단 시 ellipsis `…`(width 1)을 붙이되, `max_width == 1`이면 ellipsis를
/// 생략해 가시 1칸을 콘텐츠로 쓰고, `max_width == 0`이면 빈 문자열을 반환한다.
pub fn truncate_status_line_ansi(line: &str, max_width: u16) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    let width = max_width as usize;
    if width == 0 {
        return String::new();
    }

    // 입력을 SGR 시퀀스(폭 0, 원자적)와 가시 grapheme cluster로 분해한다.
    // SGR은 `format_status_line`이 다루지 않던 추가 토큰이므로 여기서 별도 처리.
    let segments = split_sgr_and_text(line);

    // 1차 패스: 전체 가시 폭을 계산해 절단 필요 여부 판단.
    let total_visible_width: usize = segments
        .iter()
        .filter_map(|segment| match segment {
            StatusSegment::Text(text) => Some(text.width()),
            StatusSegment::Sgr(_) => None,
        })
        .sum();

    let mut emitted_sgr = false;
    let mut buf = String::new();

    if total_visible_width <= width {
        // 절단 불필요: 그대로 재조립.
        for segment in &segments {
            match segment {
                StatusSegment::Sgr(seq) => {
                    buf.push_str(seq);
                    emitted_sgr = true;
                }
                StatusSegment::Text(text) => buf.push_str(text),
            }
        }
    } else {
        // 절단 필요: `…` 한 칸 예약(width >= 2일 때만), grapheme 단위 누적.
        let ellipsis_margin: usize = if width >= 2 { 1 } else { 0 };
        let target = width.saturating_sub(ellipsis_margin);
        let mut acc = 0_usize;
        // 텍스트 절단 지점 이후의 SGR은 더 push하지 않는다(잘린 영역 색 누설 방지).
        // 일단 절단되면 보이는 콘텐츠가 더 붙지 않으므로, 이후 색 변경을 emit하면
        // 끝의 `\x1b[0m` 직전에 무의미한 색만 남아 누설된다. truncated 시점에 중단한다.
        let mut truncated = false;
        for segment in &segments {
            if truncated {
                break;
            }
            match segment {
                StatusSegment::Sgr(seq) => {
                    // SGR은 폭 0. 단, 이미 가용 폭을 다 쓴(acc >= target) 뒤의 SGR은
                    // 더 표시될 콘텐츠가 없어 잘린 영역 색 누설일 뿐이므로 push하지 않는다.
                    if acc < target {
                        buf.push_str(seq);
                        emitted_sgr = true;
                    } else {
                        truncated = true;
                    }
                }
                StatusSegment::Text(text) => {
                    for cluster in text.graphemes(true) {
                        let cluster_width = cluster.width();
                        if acc + cluster_width > target {
                            // 이 텍스트(및 이후 모든 세그먼트)는 표시 폭을 넘겨 잘린다.
                            truncated = true;
                            break;
                        }
                        buf.push_str(cluster);
                        acc += cluster_width;
                    }
                }
            }
        }
        if ellipsis_margin > 0 {
            buf.push('…');
        }
    }

    // 색 누수 차단: SGR이 하나라도 있으면 reset으로 닫는다.
    if emitted_sgr {
        buf.push_str("\u{001b}[0m");
    }

    buf
}

/// `truncate_status_line_ansi`가 사용하는 토큰: 완결 SGR 시퀀스(폭 0) 또는 가시 텍스트.
enum StatusSegment<'a> {
    /// `\x1b[...m` 형태의 SGR 시퀀스. 폭 0으로 취급하며 원자적으로 보존한다.
    Sgr(&'a str),
    /// 가시 문자 구간. grapheme/폭 계산 대상.
    Text(&'a str),
}

/// 살균된 단일행을 SGR 시퀀스와 가시 텍스트 구간으로 분해한다.
///
/// 입력은 `sanitize_status_command_line` 출력을 가정하므로 escape는 `\x1b[...m`
/// (최종 바이트 `m`) SGR뿐이다. 그 외 ESC는 나타나지 않지만, 방어적으로 `m`을
/// 만나지 못하면 남은 전체를 SGR로 흡수해 ESC 누수를 막는다.
fn split_sgr_and_text(line: &str) -> Vec<StatusSegment<'_>> {
    let bytes = line.as_bytes();
    let mut segments: Vec<StatusSegment<'_>> = Vec::new();
    let mut text_start = 0_usize;
    let mut index = 0_usize;

    while index < bytes.len() {
        // `\x1b[` 로 시작하는 SGR 시퀀스 탐지.
        if bytes[index] == 0x1b && index + 1 < bytes.len() && bytes[index + 1] == b'[' {
            if text_start < index {
                segments.push(StatusSegment::Text(&line[text_start..index]));
            }
            // 최종 바이트 `m`까지 흡수.
            let mut end = index + 2;
            while end < bytes.len() && bytes[end] != b'm' {
                end += 1;
            }
            // `m`을 포함하도록 확장(존재 시).
            if end < bytes.len() {
                end += 1;
            }
            segments.push(StatusSegment::Sgr(&line[index..end]));
            index = end;
            text_start = index;
            continue;
        }
        index += 1;
    }

    if text_start < bytes.len() {
        segments.push(StatusSegment::Text(&line[text_start..]));
    }

    segments
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalCaptureState {
    escape: EscapeState,
    pending_utf8: [u8; 4],
    pending_utf8_len: u8,
}

impl Default for TerminalCaptureState {
    fn default() -> Self {
        Self {
            escape: EscapeState::Ground,
            pending_utf8: [0; 4],
            pending_utf8_len: 0,
        }
    }
}

impl TerminalCaptureState {
    fn pending_utf8(&self) -> &[u8] {
        &self.pending_utf8[..usize::from(self.pending_utf8_len)]
    }

    fn clear_pending_utf8(&mut self) {
        self.pending_utf8_len = 0;
    }

    fn store_incomplete_utf8(&mut self, bytes: &[u8]) -> bool {
        let Some(first) = bytes.first().copied() else {
            return false;
        };
        let Some(width) = utf8_char_width(first) else {
            return false;
        };
        if bytes.len() >= width {
            return false;
        }
        debug_assert!(width <= self.pending_utf8.len());
        let len = bytes.len().min(self.pending_utf8.len());
        self.pending_utf8[..len].copy_from_slice(&bytes[..len]);
        self.pending_utf8_len = len as u8;
        true
    }

    fn flush_pending_utf8_at_capture_end(&mut self, out: &mut String) {
        if self.pending_utf8_len != 0 && self.escape == EscapeState::Ground {
            out.push('\u{fffd}');
        }
        self.clear_pending_utf8();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
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

    fn terminal_state_is_ground(state: TerminalCaptureState) -> bool {
        state.escape == EscapeState::Ground && state.pending_utf8_len == 0
    }

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
    fn terminal_capture_does_not_treat_utf8_continuation_as_c1_st() {
        assert_eq!(terminal_capture("A\x1b]52;c;ÜSECRET\x07B".as_bytes()), "AB");
        assert_eq!(terminal_capture("A\x1bPqÜSECRET\x1b\\B".as_bytes()), "AB");
        let text = terminal_text("SAFE_\x1b]52;c;ÜLIST_SECRET\x07_AFTER");
        assert_eq!(text, "SAFE__AFTER");
        assert!(!text.contains("LIST_SECRET"));
    }

    #[test]
    fn terminal_capture_handles_csi_utf8_and_stringesc_raw_st() {
        assert_eq!(terminal_capture("A\x1b[1ÜmX".as_bytes()), "AX");
        assert_eq!(terminal_capture("A\x1b[1\u{009c}X".as_bytes()), "AX");
        assert_eq!(terminal_capture(b"A\x1b]secret\x1b\x9cB"), "AB");
        assert_eq!(terminal_capture(b"A\x1bPqsecret\x1b\x9cB"), "AB");
    }

    #[test]
    fn terminal_text_strips_escape_payloads_and_line_controls() {
        let text = terminal_text(
            "LIST_VISIBLE_\x1b]52;c;LIST_SECRET\x07\x1bPqLIST_DCS\x1b\\_AFTER\tNEXT\nROW",
        );
        assert_eq!(text, "LIST_VISIBLE__AFTERNEXTROW");
        assert!(!text.contains("LIST_SECRET"));
        assert!(!text.contains("LIST_DCS"));
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\x07'));
    }

    #[test]
    fn terminal_capture_replaces_incomplete_or_invalid_utf8() {
        assert_eq!(terminal_capture(&[b'A', 0xe2]), "A�");
        assert_eq!(terminal_capture(&[b'A', 0xf5, b'B']), "A�B");
    }

    #[test]
    fn terminal_capture_hides_incomplete_utf8_inside_escape_state_at_eof() {
        assert_eq!(terminal_capture(b"A\x1b]52;c;\xc2"), "A");
        assert_eq!(terminal_capture(b"A\x1bPq\xc2"), "A");
        assert_eq!(terminal_capture(b"A\x1b[1\xc2"), "A");
        assert_eq!(terminal_capture(b"A\x1b]hidden\x1b\xc2"), "A");
    }

    #[test]
    fn terminal_capture_state_carries_split_utf8_text() {
        let mut state = TerminalCaptureState::default();
        let bytes = "완료".as_bytes();

        assert_eq!(terminal_capture_from_state(&bytes[..1], &mut state), "");
        assert!(
            !terminal_state_is_ground(state),
            "partial UTF-8 scalar should remain pending for the next chunk"
        );
        assert_eq!(terminal_capture_from_state(&bytes[1..3], &mut state), "완");
        assert!(terminal_state_is_ground(state));
        assert_eq!(terminal_capture_from_state(&bytes[3..5], &mut state), "");
        assert!(!terminal_state_is_ground(state));
        assert_eq!(terminal_capture_from_state(&bytes[5..], &mut state), "료");
        assert!(terminal_state_is_ground(state));
    }

    #[test]
    fn terminal_capture_state_carries_split_utf8_string_terminator() {
        let mut state = TerminalCaptureState::default();

        assert_eq!(
            terminal_capture_from_state(b"A\x1b]52;c;hidden\xc2", &mut state),
            "A"
        );
        assert!(
            !terminal_state_is_ground(state),
            "OSC plus partial UTF-8 ST should stay pending"
        );
        assert_eq!(terminal_capture_from_state(&[0x9c, b'B'], &mut state), "B");
        assert!(terminal_state_is_ground(state));
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

    // ===== sanitize_status_command_line (함수 1) =====

    #[test]
    fn sanitize_status_command_line_preserves_color_when_allowed() {
        // ① allow_color로 SGR 색이 보존되고, emit 후 reset으로 닫힌다.
        let out = sanitize_status_command_line(b"\x1b[31mred\x1b[0m", true);
        assert_eq!(out, "\x1b[31mred\x1b[0m\x1b[0m");
    }

    #[test]
    fn sanitize_status_command_line_discards_non_sgr_csi() {
        // ② non-`m` CSI(`\x1b[2J` 화면 클리어)는 폐기.
        let out = sanitize_status_command_line(b"a\x1b[2Jb", true);
        assert_eq!(out, "ab");
    }

    #[test]
    fn sanitize_status_command_line_discards_csi_with_intermediate() {
        // ③ intermediate(`0x20` 공백)가 낀 CSI는 무효 → 폐기.
        let out = sanitize_status_command_line(b"a\x1b[31 mb", true);
        assert_eq!(out, "ab");
    }

    #[test]
    fn sanitize_status_command_line_drops_incomplete_csi_without_leak() {
        // ④ 미완결 CSI(`\x1b[31` + EOF)는 아무것도 emit하지 않는다(누수 0).
        let out = sanitize_status_command_line(b"a\x1b[31", true);
        assert_eq!(out, "a");
        assert!(!out.contains('3'));
        assert!(!out.contains('1'));
    }

    #[test]
    fn sanitize_status_command_line_normalizes_c1_csi_to_esc_bracket() {
        // ⑤ C1 `\x9b31m`·UTF-8형 `\u{009b}…` SGR을 `\x1b[31m`로 정규화 통과.
        let raw_c1 = sanitize_status_command_line(b"\x9b31mred", true);
        assert_eq!(raw_c1, "\x1b[31mred\x1b[0m");

        let utf8_c1 = sanitize_status_command_line("\u{009b}31mred".as_bytes(), true);
        assert_eq!(utf8_c1, "\x1b[31mred\x1b[0m");
    }

    #[test]
    fn sanitize_status_command_line_discards_osc_payload() {
        // ⑥ OSC52 clipboard 시퀀스는 폐기(secret 누수 없음).
        let out = sanitize_status_command_line(b"a\x1b]52;c;secret\x07b", true);
        assert_eq!(out, "ab");
        assert!(!out.contains("secret"));
    }

    #[test]
    fn sanitize_status_command_line_does_not_treat_utf8_continuation_as_c1_st() {
        let out = sanitize_status_command_line("a\x1b]52;c;Üsecret\x07b".as_bytes(), true);
        assert_eq!(out, "ab");

        let dcs = sanitize_status_command_line("a\x1bPqÜsecret\x1b\\b".as_bytes(), true);
        assert_eq!(dcs, "ab");
    }

    #[test]
    fn sanitize_status_command_line_handles_csi_utf8_and_stringesc_raw_st() {
        assert_eq!(
            sanitize_status_command_line("a\x1b[1Ümb".as_bytes(), true),
            "ab"
        );
        assert_eq!(
            sanitize_status_command_line("a\x1b[1\u{009c}b".as_bytes(), true),
            "ab"
        );
        assert_eq!(
            sanitize_status_command_line(b"a\x1b]secret\x1b\x9cb", true),
            "ab"
        );
        assert_eq!(
            sanitize_status_command_line(b"a\x1bPqsecret\x1b\x9cb", true),
            "ab"
        );
    }

    #[test]
    fn sanitize_status_command_line_discards_lone_esc_and_charset() {
        // ⑦ 단독 ESC와 charset 지정 시퀀스는 폐기.
        // 입력 끝의 단독 ESC: 아무것도 emit하지 않음(누수 0).
        assert_eq!(sanitize_status_command_line(b"a\x1b", true), "a");
        // 알 수 없는 2바이트 escape(`\x1bb`): terminal_capture와 동일하게 종결 바이트
        // `b`까지 escape의 일부로 소비되어 폐기된다.
        assert_eq!(sanitize_status_command_line(b"a\x1bbX", true), "aX");
        // charset 지정(`\x1b(B`)은 3바이트 모두 폐기, 이후 텍스트는 보존.
        assert_eq!(sanitize_status_command_line(b"a\x1b(Bb", true), "ab");
    }

    #[test]
    fn sanitize_status_command_line_discards_csi_over_param_byte_limit() {
        // ⑧ 파라미터 바이트 길이 > 64면 폐기.
        let mut payload = Vec::from(&b"a\x1b["[..]);
        payload.extend(std::iter::repeat_n(b'1', 65));
        payload.push(b'm');
        payload.push(b'b');
        let out = sanitize_status_command_line(&payload, true);
        assert_eq!(out, "ab");
    }

    #[test]
    fn sanitize_status_command_line_discards_csi_over_param_count_limit() {
        // ⑨ `;`로 분리된 파라미터 개수 > 16이면 폐기(`1;1;...;1` 17개).
        let params = vec!["1"; 17].join(";");
        let payload = format!("a\x1b[{params}mb");
        let out = sanitize_status_command_line(payload.as_bytes(), true);
        assert_eq!(out, "ab");
    }

    #[test]
    fn sanitize_status_command_line_stops_at_first_newline() {
        // ⑩ multiline `a\nb` → 첫 줄 `a`만(이후 무시).
        assert_eq!(sanitize_status_command_line(b"a\nb", true), "a");
        assert_eq!(sanitize_status_command_line(b"a\rb", true), "a");
    }

    #[test]
    fn sanitize_status_command_line_stops_at_newline_in_any_parser_state() {
        // B1: LF/CR은 OSC/escape 등 비-Ground 상태에서도 정지 바이트다.
        // OSC 도중 LF: ESC ] 진입 후 newline에서 멈춰 payload가 다음 줄로 누수되지 않는다.
        let out = sanitize_status_command_line(b"\x1b]osc\npayload", true);
        assert_eq!(out, "");
        assert!(!out.contains("payload"));
        // 텍스트 뒤 OSC 도중 LF: 앞 텍스트만 남고 OSC 인자/이후 줄은 사라진다.
        let out2 = sanitize_status_command_line(b"head\x1b]52;c\nsecret", true);
        assert_eq!(out2, "head");
        assert!(!out2.contains("secret"));
        assert!(!out2.contains("52;c"));
        // CSI 도중 CR도 동일하게 정지.
        let out3 = sanitize_status_command_line(b"x\x1b[31\rrest", true);
        assert_eq!(out3, "x");
    }

    #[test]
    fn sanitize_status_command_line_replaces_invalid_utf8() {
        // ⑪ 잘못된 UTF-8 바이트는 `\u{fffd}`로 치환.
        let out = sanitize_status_command_line(&[b'A', 0xe2], true);
        assert_eq!(out, "A\u{fffd}");
        let out2 = sanitize_status_command_line(&[b'A', 0xf5, b'B'], true);
        assert_eq!(out2, "A\u{fffd}B");
    }

    #[test]
    fn sanitize_status_command_line_strips_all_escapes_when_color_disabled() {
        // ⑫ allow_color=false면 SGR까지 전량 strip(plain 단일행).
        let out = sanitize_status_command_line(b"\x1b[31mred\x1b[0m", false);
        assert_eq!(out, "red");
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn sanitize_status_command_line_allows_extended_sgr_but_rejects_private() {
        // ⑬ 유효 256색·트루컬러 콜론형은 통과, private(`?`)는 폐기.
        let out256 = sanitize_status_command_line(b"\x1b[38;5;200mx", true);
        assert_eq!(out256, "\x1b[38;5;200mx\x1b[0m");

        let truecolor = sanitize_status_command_line(b"\x1b[38:2:1:2:3mx", true);
        assert_eq!(truecolor, "\x1b[38:2:1:2:3mx\x1b[0m");

        // private `\x1b[?25h`(커서 표시)는 무효 → 폐기, reset도 없음.
        let private = sanitize_status_command_line(b"a\x1b[?25hb", true);
        assert_eq!(private, "ab");
        assert!(!private.contains('\x1b'));
    }

    // ===== truncate_status_line_ansi (함수 2) =====

    #[test]
    fn truncate_status_line_ansi_truncates_plain_with_ellipsis() {
        // ⑭ plain 폭 절단 + `…`(width=5: 4칸 콘텐츠 + ellipsis).
        let out = truncate_status_line_ansi("abcdefgh", 5);
        assert_eq!(out, "abcd…");
    }

    #[test]
    fn truncate_status_line_ansi_counts_cjk_as_two_cells() {
        // ⑮ CJK는 2폭. `한국어`(6폭)를 width=4로 자르면 ellipsis 1 + 한 글자(2폭).
        let out = truncate_status_line_ansi("한국어", 4);
        assert_eq!(out, "한…");
    }

    #[test]
    fn truncate_status_line_ansi_preserves_zwj_emoji_grapheme() {
        // ⑯ ZWJ 가족 이모지를 grapheme cluster로 보존(부분 cluster 잔존 금지).
        let family = "👨\u{200d}👩\u{200d}👧";
        // 충분한 폭에서는 cluster가 통째로 살아남는다.
        let out = truncate_status_line_ansi(&format!("{family}x"), 10);
        assert!(out.contains(family));
    }

    #[test]
    fn truncate_status_line_ansi_treats_sgr_as_zero_width() {
        // ⑰ SGR은 폭 0으로 통과, 가시 문자만 카운트(색이 폭을 잡아먹지 않음).
        // 가시 4칸 + 색 시퀀스만. SGR이 폭을 잡아먹지 않으므로 width=4에 정확히 맞아
        // 절단이 일어나지 않는다. 색 → 텍스트 순서로 emit되어 가시 문자만 카운트됨을 확인.
        let line = "\x1b[31mabcd";
        let out = truncate_status_line_ansi(line, 4);
        // 절단 없음: 색 보존 + 누수 차단 reset 부착.
        assert_eq!(out, "\x1b[31mabcd\x1b[0m");
        // SGR 4개를 텍스트 사이에 끼워도 가시 폭은 동일(4칸).
        let interleaved = "\x1b[31ma\x1b[32mb\x1b[33mc\x1b[34md";
        let out2 = truncate_status_line_ansi(interleaved, 4);
        assert_eq!(out2, "\x1b[31ma\x1b[32mb\x1b[33mc\x1b[34md\x1b[0m");
    }

    #[test]
    fn truncate_status_line_ansi_never_leaves_dangling_esc() {
        // ⑱ 임의 폭에서 절단해도 미완결 ESC 0 + SGR 있으면 끝에 `\x1b[0m`.
        let line = "\x1b[31mabcdefgh\x1b[0m";
        for max_width in 0_u16..=12 {
            let out = truncate_status_line_ansi(line, max_width);
            // ESC가 있으면 마지막 escape는 반드시 완결된 `m`으로 끝나야 한다.
            if let Some(last_esc) = out.rfind('\x1b') {
                let tail = &out[last_esc..];
                assert!(
                    tail.ends_with('m'),
                    "max_width={max_width} left dangling ESC: {out:?}"
                );
            }
            // SGR이 하나라도 있으면 reset으로 닫혀야 한다.
            if out.contains('\x1b') {
                assert!(
                    out.ends_with("\x1b[0m"),
                    "max_width={max_width} missing closing reset: {out:?}"
                );
            }
        }
    }

    #[test]
    fn truncate_status_line_ansi_drops_sgr_after_truncation_point() {
        // B2: 텍스트 절단 이후의 SGR은 push하지 않아 잘린 영역 색이 누설되지 않는다.
        // `\x1b[31mAAAA\x1b[32mBBBB` width 4 → target 3(ellipsis 1칸 예약).
        // 출력은 `\x1b[31mAAA…\x1b[0m`이고 `\x1b[32m`은 포함되지 않으며 끝은 reset.
        let out = truncate_status_line_ansi("\x1b[31mAAAA\x1b[32mBBBB", 4);
        assert!(
            !out.contains("\x1b[32m"),
            "truncated-region SGR must not leak: {out:?}"
        );
        assert!(out.ends_with("\x1b[0m"), "must close with reset: {out:?}");
        assert_eq!(out, "\x1b[31mAAA…\x1b[0m");
    }

    #[test]
    fn truncate_status_line_ansi_returns_empty_for_zero_width() {
        // ⑲ max_width=0 → 빈 문자열.
        assert_eq!(truncate_status_line_ansi("abc", 0), "");
        assert_eq!(truncate_status_line_ansi("\x1b[31mabc\x1b[0m", 0), "");
    }

    #[test]
    fn truncate_status_line_ansi_width_one_shows_visible_cell_without_ellipsis() {
        // ⑳ max_width=1 → ellipsis 생략, 가시 1칸 표시.
        assert_eq!(truncate_status_line_ansi("abc", 1), "a");
        // SGR 포함 시에도 가시 1칸 + 닫힘. (color → text 1칸, ellipsis 생략)
        let out = truncate_status_line_ansi("\x1b[31mabc", 1);
        assert_eq!(out, "\x1b[31ma\x1b[0m");
    }
}
