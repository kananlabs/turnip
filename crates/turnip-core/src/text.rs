//! Text and formatting utilities shared by tools, sessions, and rendering.

use std::io::Read;

/// Replace invalid UTF-8 and problematic control characters so terminal
/// output and model context stay well formed. ANSI escape sequences are
/// removed whole (a lone stripped ESC would leave their text behind, and a
/// live one would let tool output move the cursor), `\r` is dropped, other
/// C0 controls except `\n` and `\t` are removed, and `\t` expands to
/// spaces.
pub fn sanitize_utf8(input: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(&strip_ansi(input)).into_owned();
    if text.contains('\t') {
        text = text.replace('\t', "    ");
    }
    text.chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

/// Remove CSI/OSC escape sequences and other ESC-prefixed sequences.
fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != 0x1b {
            out.push(input[index]);
            index += 1;
            continue;
        }
        match input.get(index + 1) {
            Some(b'[') => {
                // CSI: parameters 0x20-0x3F then a final byte 0x40-0x7E.
                index += 2;
                while index < input.len() && !(0x40..=0x7e).contains(&input[index]) {
                    index += 1;
                }
                index += 1;
            }
            Some(b']') => {
                // OSC: until BEL or ST.
                index += 2;
                while index < input.len() {
                    if input[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if input[index] == 0x1b && input.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            Some(_) => index += 2, // ESC + one byte
            None => break,
        }
    }
    out
}

/// Sanitize already-decoded text the same way [`sanitize_utf8`] treats bytes.
pub fn sanitize_str(text: &str) -> String {
    sanitize_utf8(text.as_bytes())
}

/// Truncate a byte buffer to `cap` bytes, keeping the head and tail with an
/// elision marker in the middle. Input larger than the cap but smaller than
/// twice the cap keeps a proportionally larger tail. Never grows the input.
pub fn truncate_head_tail(bytes: &[u8], cap: usize) -> Vec<u8> {
    if bytes.len() <= cap {
        return bytes.to_vec();
    }
    const MARKER: &[u8] = b"\n...[truncated]\n...\n";
    let budget = cap.saturating_sub(MARKER.len());
    let head = budget / 2;
    let tail = budget - head;
    let mut out = Vec::with_capacity(cap);
    out.extend_from_slice(&bytes[..head]);
    out.extend_from_slice(MARKER);
    out.extend_from_slice(&bytes[bytes.len() - tail..]);
    out
}

/// Truncate a string by display columns (approximate: per-character width),
/// appending an ellipsis when cut.
pub fn truncate_columns(text: &str, columns: usize) -> String {
    if columns == 0 {
        return String::new();
    }
    let mut used = 0usize;
    let mut out = String::new();
    for ch in text.chars() {
        let width = char_width(ch).unwrap_or(0);
        if used + width > columns.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(ch);
        used += width;
    }
    out
}

/// Display width of a character, treating ambiguous widths as narrow.
pub fn char_width(ch: char) -> Option<usize> {
    use unicode_width::UnicodeWidthChar;
    ch.width()
}

/// Display width of a string, ignoring ANSI escapes is *not* handled here;
/// callers measuring styled text must strip escapes first.
pub fn str_width(text: &str) -> usize {
    text.chars().map(|c| char_width(c).unwrap_or(0)).sum()
}

/// Wrap a single logical line into display lines of at most `width` columns,
/// breaking on spaces when possible.
pub fn wrap_line(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for word in text.split(' ') {
        let word_width = str_width(word);
        if used > 0 && used + 1 + word_width > width {
            lines.push(std::mem::take(&mut current));
            used = 0;
        }
        if word_width > width {
            // Hard-break an oversized word.
            let mut piece = String::new();
            let mut piece_width = 0;
            for ch in word.chars() {
                let cw = char_width(ch).unwrap_or(0);
                if piece_width + cw > width {
                    lines.push(std::mem::take(&mut piece));
                    piece_width = 0;
                }
                piece.push(ch);
                piece_width += cw;
            }
            current = piece;
            used = piece_width;
        } else {
            if used > 0 {
                current.push(' ');
                used += 1;
            }
            current.push_str(word);
            used += word_width;
        }
    }
    lines.push(current);
    lines
}

/// Render a unified diff of two texts, `context_lines` of context around each
/// change. Lines are prefixed with ' ', '-', or '+'.
pub fn unified_diff(old: &str, new: &str, context_lines: usize) -> String {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    for group in diff.grouped_ops(context_lines) {
        for op in group {
            for change in diff.iter_changes(&op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal => ' ',
                };
                let mut line = change.to_string_lossy().into_owned();
                if !line.ends_with('\n') {
                    line.push('\n');
                }
                out.push(sign);
                out.push_str(&line);
            }
        }
    }
    out
}

/// Count added and removed lines in a unified diff produced by
/// [`unified_diff`].
pub fn diff_counts(diff: &str) -> (usize, usize) {
    let added = diff.lines().filter(|l| l.starts_with('+')).count();
    let removed = diff.lines().filter(|l| l.starts_with('-')).count();
    (added, removed)
}

/// Read up to `limit` bytes from a reader, or return an error when it
/// overflows. Used to reject oversized files before they reach memory.
pub fn read_bounded(mut reader: impl Read, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Ok(buf);
        }
        if buf.len() as u64 + n as u64 > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "file exceeds size limit",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// First line of a prompt for session labels, capped to `max_columns`.
pub fn prompt_summary(text: &str, max_columns: usize) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let flat = first.replace('\t', " ");
    truncate_columns(flat.trim(), max_columns)
}

/// Sort helper: newest-first by (seconds, nanoseconds).
pub fn newest_first_cmp(a_secs: i64, a_nanos: i64, b_secs: i64, b_nanos: i64) -> std::cmp::Ordering {
    (a_secs, a_nanos).cmp(&(b_secs, b_nanos)).reverse()
}

/// Format an elapsed duration compactly: `1.2s`, `3m41s`, `2h05m`.
pub fn format_elapsed(ms: i64) -> String {
    if ms < 0 {
        return "-".into();
    }
    let total_seconds = ms / 1000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3600;
    if hours > 0 {
        format!("{h}h{m:02}m", h = hours, m = minutes)
    } else if minutes > 0 {
        format!("{m}m{s:02}s", m = minutes, s = seconds)
    } else {
        format!("{s}.{t}s", s = seconds, t = (ms % 1000) / 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_controls() {
        assert_eq!(sanitize_utf8(b"a\x00b\x1b[31mc\rd\ne"), "abcd\ne");
        assert_eq!(sanitize_utf8(b"\x1b]0;title\x07plain"), "plain");
    }

    #[test]
    fn truncation_keeps_head_and_tail() {
        let data = (0..=99u8).collect::<Vec<u8>>();
        let cut = truncate_head_tail(&data, 40);
        assert!(cut.len() <= 40);
        assert!(cut.windows(11).any(|w| w == b"[truncated]"));
        assert_eq!(cut.first(), Some(&0));
        assert_eq!(cut.last(), Some(&99));
    }

    #[test]
    fn wraps_to_width() {
        let lines = wrap_line("alpha beta gamma delta", 11);
        assert!(lines.iter().all(|l| str_width(l) <= 11));
        assert_eq!(lines, vec!["alpha beta", "gamma delta"]);
    }

    #[test]
    fn diff_reports_changes() {
        let diff = unified_diff("a\nb\n", "a\nc\n", 3);
        let (added, removed) = diff_counts(&diff);
        assert_eq!((added, removed), (1, 1));
    }

    #[test]
    fn elapsed_formats() {
        assert_eq!(format_elapsed(9400), "9.4s");
        assert_eq!(format_elapsed(221_000), "3m41s");
        assert_eq!(format_elapsed(7_500_000), "2h05m");
    }
}
