//! The live region has one anchor: its top row. All output starts there,
//! and every repaint returns there. Only writing new content may scroll.

use std::io::{self, Write};

use crossterm::{cursor, queue, terminal};

use super::StyledLine;

#[derive(Default)]
pub(super) struct Region {
    rows: usize,
    cursor_row: usize,
}

/// Hard-wrap without changing spaces (input and code must retain them).
pub(super) fn wrap_line(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut column = 0;
    for mut ch in text.chars() {
        let mut cells = turnip_core::text::char_width(ch).unwrap_or(0);
        if cells > width {
            ch = '�';
            cells = 1;
        }
        if column + cells > width {
            rows.push(String::new());
            column = 0;
        }
        rows.last_mut().unwrap().push(ch);
        column += cells;
    }
    rows
}

/// Expand logical lines before counting rows. Never let untrusted text send
/// cursor commands, or let an embedded newline bypass the row accounting.
fn screen_lines(lines: &[StyledLine], width: usize) -> Vec<StyledLine> {
    let mut rows = Vec::new();
    for line in lines {
        let text = turnip_core::text::sanitize_str(&line.text);
        for logical in text.split('\n') {
            for row in wrap_line(logical, width) {
                rows.push(StyledLine::styled(row, line.style));
            }
        }
    }
    rows
}

impl Region {
    fn top(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.cursor_row > 0 {
            queue!(out, cursor::MoveUp(self.cursor_row as u16))?;
        }
        queue!(out, cursor::MoveToColumn(0))?;
        self.cursor_row = 0;
        Ok(())
    }

    pub fn clear(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.top(out)?;
        queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;
        self.rows = 0;
        Ok(())
    }

    pub fn push(
        &mut self,
        out: &mut impl Write,
        lines: &[StyledLine],
        width: usize,
    ) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        self.clear(out)?;
        for line in screen_lines(lines, width) {
            out.write_all(line.render().as_bytes())?;
            out.write_all(b"\r\n")?;
        }
        out.flush()
    }

    pub fn render(
        &mut self,
        out: &mut impl Write,
        lines: &[StyledLine],
        (width, height): (usize, usize),
    ) -> io::Result<()> {
        let rows = screen_lines(lines, width.max(1));
        let visible = &rows[rows.len().saturating_sub(height.max(1))..];
        self.clear(out)?;
        for (index, line) in visible.iter().enumerate() {
            if index > 0 {
                out.write_all(b"\r\n")?;
            }
            out.write_all(line.render().as_bytes())?;
        }
        self.rows = visible.len();
        self.cursor_row = self.rows.saturating_sub(1);
        self.top(out)?;
        out.flush()
    }

    pub fn cursor(
        &mut self,
        out: &mut impl Write,
        row: usize,
        column: usize,
        width: usize,
    ) -> io::Result<()> {
        self.top(out)?;
        self.cursor_row = row.min(self.rows.saturating_sub(1));
        if self.cursor_row > 0 {
            queue!(out, cursor::MoveDown(self.cursor_row as u16))?;
        }
        queue!(
            out,
            cursor::MoveToColumn(column.min(width.saturating_sub(1)) as u16)
        )?;
        out.flush()
    }

    pub fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.top(out)?;
        if self.rows > 1 {
            queue!(out, cursor::MoveDown(self.rows as u16 - 1))?;
        }
        out.write_all(b"\r\n")?;
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Screen {
        region: Region,
        parser: vt100::Parser,
        bytes: Vec<u8>,
        size: (usize, usize),
    }

    impl Screen {
        fn new(width: usize, height: usize) -> Self {
            Self {
                region: Region::default(),
                parser: vt100::Parser::new(height as u16, width as u16, 100),
                bytes: Vec::new(),
                size: (width, height),
            }
        }

        fn flush(&mut self) {
            self.parser.process(&self.bytes);
            self.bytes.clear();
        }

        fn render(&mut self, lines: &[StyledLine]) {
            self.region
                .render(&mut self.bytes, lines, self.size)
                .unwrap();
            self.flush();
        }

        fn cursor(&mut self, row: usize, column: usize) {
            self.region
                .cursor(&mut self.bytes, row, column, self.size.0)
                .unwrap();
            self.flush();
        }

        fn push(&mut self, text: &str) {
            self.region
                .push(&mut self.bytes, &[StyledLine::plain(text)], self.size.0)
                .unwrap();
            self.flush();
        }

        fn contents(&self) -> String {
            self.parser.screen().contents()
        }
    }

    #[test]
    fn wrapped_region_does_not_walk_or_scroll_on_repaint() {
        let mut screen = Screen::new(12, 8);
        screen.push("transcript");
        let lines = [
            StyledLine::plain("a long picker title that wraps"),
            StyledLine::plain("/filter"),
        ];
        screen.render(&lines);
        screen.cursor(3, 4);
        let expected = screen.contents();
        let cursor = screen.parser.screen().cursor_position();
        for _ in 0..100 {
            screen.render(&lines);
            screen.cursor(3, 4);
            assert_eq!(screen.contents(), expected);
            assert_eq!(screen.parser.screen().cursor_position(), cursor);
        }
        assert!(screen.contents().starts_with("transcript\n"));
    }

    #[test]
    fn full_screen_region_can_shrink_and_grow_without_scrolling() {
        let mut screen = Screen::new(8, 4);
        let large: Vec<_> = ["one", "two", "three", "four"]
            .map(StyledLine::plain)
            .into();
        screen.render(&large);
        let expected = screen.contents();
        for _ in 0..100 {
            screen.render(&[StyledLine::plain("short")]);
            assert_eq!(screen.contents(), "short");
            screen.render(&large);
            assert_eq!(screen.contents(), expected);
        }
    }

    #[test]
    fn repaint_at_the_bottom_does_not_add_scrollback() {
        let mut screen = Screen::new(10, 4);
        screen.push("history 1\nhistory 2\nhistory 3");
        let lines = [
            StyledLine::plain("title"),
            StyledLine::plain("filter"),
            StyledLine::plain("item"),
        ];
        screen.render(&lines);
        let expected = screen.contents();
        let mut history = screen.parser.screen().clone();
        history.set_scrollback(usize::MAX);
        let scrollback = history.scrollback();
        assert!(scrollback > 0);
        for _ in 0..100 {
            screen.render(&lines);
            assert_eq!(screen.contents(), expected);
        }
        let mut history = screen.parser.screen().clone();
        history.set_scrollback(usize::MAX);
        assert_eq!(history.scrollback(), scrollback);
    }

    #[test]
    fn resizing_keeps_subsequent_repaints_stable() {
        let mut screen = Screen::new(30, 10);
        let lines = [
            StyledLine::plain("a long title with details"),
            StyledLine::plain("filter"),
            StyledLine::plain("item"),
        ];
        screen.render(&lines);
        screen.cursor(1, 4);
        screen.size = (8, 4);
        screen.parser.screen_mut().set_size(4, 8);
        screen.render(&lines);
        let expected = screen.contents();
        for _ in 0..20 {
            screen.render(&lines);
            assert_eq!(screen.contents(), expected);
        }
    }

    #[test]
    fn push_erases_the_live_region_before_committing_content() {
        let mut screen = Screen::new(20, 8);
        screen.push("history");
        screen.render(&[
            StyledLine::plain("old long status"),
            StyledLine::plain("old input"),
        ]);
        screen.cursor(1, 4);
        screen.push("new");
        screen.render(&[StyledLine::plain("prompt")]);
        assert_eq!(screen.contents(), "history\nnew\nprompt");
    }

    #[test]
    fn embedded_newlines_and_controls_cannot_escape_row_accounting() {
        let mut screen = Screen::new(10, 6);
        let lines = [StyledLine::plain("first\n\x1b[2Jsecond\r\nthird\t!")];
        screen.render(&lines);
        let expected = screen.contents();
        for _ in 0..10 {
            screen.render(&lines);
            assert_eq!(screen.contents(), expected);
        }
        assert!(expected.starts_with("first\nsecond\nthird"));
    }

    #[test]
    fn overflow_is_clipped_in_physical_rows() {
        let mut screen = Screen::new(5, 3);
        let lines = [
            StyledLine::plain("aaaaabbbbbcccccddddd"),
            StyledLine::plain("last"),
        ];
        screen.render(&lines);
        assert_eq!(screen.region.rows, 3);
        assert_eq!(screen.contents(), "ccccc\nddddd\nlast");
        for _ in 0..10 {
            screen.render(&lines);
            assert_eq!(screen.contents(), "ccccc\nddddd\nlast");
        }
    }

    #[test]
    fn cursor_placement_is_absolute_within_the_region_and_clamped() {
        let mut screen = Screen::new(10, 5);
        screen.render(&[StyledLine::plain("a"), StyledLine::plain("b")]);
        screen.cursor(1, 4);
        screen.cursor(1, 4);
        assert_eq!(screen.parser.screen().cursor_position(), (1, 4));
        screen.cursor(100, 100);
        assert_eq!(screen.parser.screen().cursor_position(), (1, 9));
        screen.render(&[]);
        assert_eq!(screen.contents(), "");
        assert_eq!(screen.parser.screen().cursor_position(), (0, 0));
    }
}
