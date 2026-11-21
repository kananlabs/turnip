//! Markdown rendering for the terminal, built for streaming: lines that are
//! complete (and not inside an open code block) can be flushed into the
//! scrollback, and only the tail re-renders in the live region.

use super::{StyledLine, LineStyle};

/// Render `text` (markdown) into styled terminal lines, each wrapped to
/// `width` display columns. Supports what streams actually contain: fenced
/// code blocks, headings, bullet and numbered lists, block quotes, inline
/// code, bold, and links.
///
/// Built for newline-terminated streaming chunks: a trailing newline ends
/// the last line instead of opening a stray empty one (so consecutive flushes
/// never stack blank lines), and runs of blank lines collapse into the
/// single break that separates paragraphs. A break the chunk ends on is
/// kept — the text that follows in the next chunk continues under it —
/// while code fences preserve blank lines verbatim.
pub fn render(text: &str, width: usize) -> Vec<StyledLine> {
    let mut lines = Vec::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_lines: Vec<String> = Vec::new();
    // A paragraph break waits for the content it separates; it is written
    // once, before whatever follows it.
    let mut pending_break = false;

    // The newline that ends the last line is a line ending, not an empty
    // line of its own.
    let body = text.strip_suffix('\n').unwrap_or(text);
    for raw in body.split('\n') {
        let trimmed = raw.trim_start();
        let indent = raw.len() - trimmed.len();
        let indent: String = " ".repeat(indent.min(16));

        if trimmed.starts_with("```") {
            if in_code {
                push_code_block(&mut lines, &code_lines, &code_lang, width);
                code_lines.clear();
                code_lang.clear();
                in_code = false;
            } else {
                if pending_break {
                    lines.push(StyledLine::plain(""));
                    pending_break = false;
                }
                in_code = true;
                code_lang = trimmed.trim_start_matches('`').trim().to_string();
            }
            continue;
        }
        if in_code {
            // Inside a fence, blank lines are content.
            code_lines.push(raw.to_string());
            continue;
        }
        if trimmed.is_empty() {
            pending_break = true;
            continue;
        }
        if pending_break {
            lines.push(StyledLine::plain(""));
            pending_break = false;
        }
        // Headings.
        if let Some(rest) = trimmed.strip_prefix("###") {
            lines.push(StyledLine::styled(
                format!("{indent}{}", rest.trim()),
                LineStyle {
                    bold: true,
                    ..Default::default()
                },
            ));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("##") {
            lines.push(StyledLine::styled(
                format!("{indent}{}", rest.trim()),
                LineStyle {
                    bold: true,
                    accent: true,
                    ..Default::default()
                },
            ));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            lines.push(StyledLine::styled(
                format!("{indent}{}", rest.trim()),
                LineStyle {
                    bold: true,
                    accent: true,
                    ..Default::default()
                },
            ));
            continue;
        }
        // Block quote.
        if let Some(rest) = trimmed.strip_prefix('>') {
            let rendered = render_inline(rest.trim(), width.saturating_sub(2));
            lines.push(StyledLine::styled(
                format!("{indent}│ {rendered}"),
                LineStyle {
                    dim: true,
                    ..Default::default()
                },
            ));
            continue;
        }
        // Bullet list (also handles "-").
        if trimmed.starts_with("* ") || trimmed.starts_with("- ") {
            let rest = &trimmed[2..];
            let rendered = render_inline(rest, width.saturating_sub(indent.len() + 2));
            lines.push(StyledLine::plain(format!("{indent}• {rendered}")));
            continue;
        }
        // Numbered list: "12. item" keeps its number aligned.
        let number_len = trimmed
            .find(". ")
            .filter(|dot| trimmed[..*dot].chars().all(|c| c.is_ascii_digit()))
            .map(|dot| dot + 2)
            .unwrap_or(0);
        if number_len > 0 {
            let marker: String = trimmed[..number_len].to_string();
            let rest = &trimmed[number_len..];
            let rendered = render_inline(rest, width.saturating_sub(indent.len() + number_len));
            lines.push(StyledLine::plain(format!("{indent}{marker}{rendered}")));
            continue;
        }
        let rendered = render_inline(trimmed, width.saturating_sub(indent.len()));
        lines.push(StyledLine::plain(format!("{indent}{rendered}")));
    }

    if !code_lines.is_empty() || in_code {
        if pending_break {
            lines.push(StyledLine::plain(""));
            pending_break = false;
        }
        // A fence still open ends mid-stream: close the box where it
        // stands, so the live region (and a finished remainder) shows an
        // open block instead of losing it.
        push_code_block(&mut lines, &code_lines, &code_lang, width);
    }
    // A break the chunk ends on is kept: it separates this chunk from the
    // one that follows. (`Repl::finish` trims it from a final remainder.)
    if pending_break {
        lines.push(StyledLine::plain(""));
    }
    lines
}

fn push_code_block(lines: &mut Vec<StyledLine>, code: &[String], lang: &str, width: usize) {
    let label = if lang.is_empty() {
        String::new()
    } else {
        format!(" {lang}")
    };
    lines.push(StyledLine::styled(
        format!("┌─{label}{}", "─".repeat(width.saturating_sub(label.len() + 4))),
        LineStyle {
            dim: true,
            ..Default::default()
        },
    ));
    for line in code {
        // Code keeps its exact spacing: hard-wrap instead of reflowing on
        // spaces, which would eat the indentation.
        for wrapped in super::region::wrap_line(line, width.saturating_sub(4)) {
            lines.push(StyledLine::styled(
                format!("│ {wrapped}"),
                LineStyle {
                    code: true,
                    ..Default::default()
                },
            ));
        }
    }
    lines.push(StyledLine::styled(
        format!("└{}", "─".repeat(width.saturating_sub(1))),
        LineStyle {
            dim: true,
            ..Default::default()
        },
    ));
}

/// Inline constructs: `code`, **bold**, *italic*, and [links](url).
fn render_inline(text: &str, width: usize) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        match ch {
            '`' => {
                if let Some(end) = chars[index + 1..].iter().position(|c| *c == '`') {
                    let code: String = chars[index + 1..index + 1 + end].iter().collect();
                    if turnip_core::text::str_width(&code) + out.chars().count() + 2 <= width {
                        out.push_str(&code);
                        index += end + 2;
                        continue;
                    }
                }
                out.push(ch);
            }
            '*' | '_' => {
                // Bold (**/__), then emphasis (*/_).
                let mut consumed = false;
                if chars.get(index + 1) == Some(&ch) {
                    let mut scan = index + 2;
                    while scan + 1 < chars.len() {
                        if chars[scan] == ch && chars[scan + 1] == ch {
                            let inner: String = chars[index + 2..scan].iter().collect();
                            if turnip_core::text::str_width(&inner) + out.chars().count() <= width {
                                out.push_str(&inner);
                                index = scan + 2;
                                consumed = true;
                            }
                            break;
                        }
                        scan += 1;
                    }
                } else if let Some(end) = chars[index + 1..].iter().position(|c| *c == ch) {
                    let inner: String = chars[index + 1..index + 1 + end].iter().collect();
                    if turnip_core::text::str_width(&inner) + out.chars().count() <= width {
                        out.push_str(&inner);
                        index += end + 2;
                        consumed = true;
                    }
                }
                if consumed {
                    continue;
                }
                out.push(ch);
            }
            '[' => {
                // [label](url) renders as label (url) until true links exist.
                if let Some(close) = chars[index + 1..].iter().position(|c| *c == ']') {
                    if chars.get(index + close + 2) == Some(&'(') {
                        if let Some(paren) = chars[index + close + 3..].iter().position(|c| *c == ')') {
                            let label: String = chars[index + 1..index + 1 + close].iter().collect();
                            let url: String =
                                chars[index + close + 3..index + close + 3 + paren].iter().collect();
                            let rendered = format!("{label} ({url})");
                            if turnip_core::text::str_width(&rendered) + out.chars().count() <= width {
                                out.push_str(&rendered);
                                index += close + paren + 4;
                                continue;
                            }
                        }
                    }
                }
                out.push(ch);
            }
            _ => out.push(ch),
        }
        index += 1;
    }
    out
}

/// Split streamed markdown into a flushable prefix and a live tail.
/// The prefix ends at the last complete line that is outside any open code
/// fence; the tail keeps everything that may still change.
pub fn split_for_flush(text: &str) -> (usize, usize) {
    let mut tail_start = 0;
    let mut fences = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            fences += 1;
        }
        // A trailing newline is a safe flush point when no open fence
        // crosses it.
        if line.ends_with('\n') && fences % 2 == 0 {
            tail_start = offset + line.len();
        }
        offset += line.len();
    }
    (tail_start, text.len() - tail_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_structure() {
        let text = "# Title\n\nplain *bold* text\n\n- item one\n- item two\n\n1. first\n2. second\n";
        let lines = render(text, 60);
        assert_eq!(lines[0].text, "Title");
        assert!(lines[0].style.bold && lines[0].style.accent);
        assert_eq!(lines[2].text, "plain bold text");
        assert_eq!(lines[4].text, "• item one");
        assert_eq!(lines[7].text, "1. first");
    }

    #[test]
    fn newline_terminated_chunks_commit_no_stray_blank() {
        // Flushes are newline-terminated; a trailing newline must not
        // open an empty line, or every flush would stack a blank row.
        fn texts(text: &str) -> Vec<String> {
            render(text, 40).iter().map(|l| l.text.clone()).collect()
        }
        assert_eq!(texts("one\n"), vec!["one"]);
        assert_eq!(texts("one\n\ntwo\n"), vec!["one", "", "two"]);
        // A chunk that is nothing but the paragraph break itself commits
        // exactly one blank line, not two.
        assert_eq!(texts("\n"), vec![""]);
    }

    #[test]
    fn blank_runs_collapse_to_one_break() {
        fn texts(text: &str) -> Vec<String> {
            render(text, 40).iter().map(|l| l.text.clone()).collect()
        }
        assert_eq!(texts("a\n\n\n\nb"), vec!["a", "", "b"]);
        // A break the chunk ends on is kept for the chunk that follows.
        assert_eq!(texts("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn fences_keep_their_blank_lines_verbatim() {
        let lines = render("para\n\n```rust\na\n\nb\n```\nafter\n", 40);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts[0], "para");
        assert_eq!(texts[1], "");
        assert!(texts[2].starts_with("┌─ rust"));
        assert_eq!(texts[3], "│ a");
        assert_eq!(texts[4], "│ ");
        assert_eq!(texts[5], "│ b");
        assert!(texts[6].starts_with("└"));
        assert_eq!(texts[7], "after");
    }

    #[test]
    fn renders_code_blocks() {
        let text = "before\n```rust\nfn main() {}\n```\nafter";
        let lines = render(text, 40);
        let joined: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert!(joined.iter().any(|l| l.starts_with("┌─ rust")));
        assert!(joined.iter().any(|l| *l == "│ fn main() {}"));
        assert!(joined.iter().any(|l| l.starts_with("└")));
        assert_eq!(*joined.last().unwrap(), "after");
    }

    #[test]
    fn flush_split_keeps_open_fence_in_tail() {
        let complete = "line one\nline two\n";
        let (start, _) = split_for_flush(complete);
        assert_eq!(start, complete.len());

        let open_fence = "text\n```rust\ncode line\n";
        let (start, _) = split_for_flush(open_fence);
        // Only the newline before the fence is safe to flush.
        assert_eq!(start, "text\n".len());

        let closed = "text\n```rust\nx\n```\ntail";
        let (start, _) = split_for_flush(closed);
        assert_eq!(start, "text\n```rust\nx\n```\n".len());
    }

    #[test]
    fn inline_rendering_drops_markers() {
        assert_eq!(render_inline("`code` and **bold**", 60), "code and bold");
        assert_eq!(render_inline("[label](http://x)", 60), "label (http://x)");
        assert_eq!(render_inline("a_b stays", 60), "a_b stays");
    }
}
