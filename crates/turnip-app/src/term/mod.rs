//! Terminal handling: raw mode, the scrollback/live-region rendering model,
//! key event pumping, and the input line editor.
//!
//! Like hax, turnip never takes over the screen. Completed content is
//! written into the terminal's native scrollback; only a bounded live region
//! (streaming tail, status line, input) is redrawn, and Ctrl+C stays
//! cooperative everywhere.

pub mod markdown;
pub mod picker;
mod region;

use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::{cursor, execute, style, terminal};

/// Prompt shown before the user's input.
pub const PROMPT: &str = "› ";
/// How often the tick loop polls terminal events.
pub const POLL_INTERVAL: Duration = Duration::from_millis(30);

/// A styled terminal line: plain UTF-8 with SGR attributes applied by the
/// renderer, kept simple so lines stay cheap to compare and print.
#[derive(Debug, Clone, PartialEq)]
pub struct StyledLine {
    pub text: String,
    pub style: LineStyle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LineStyle {
    pub dim: bool,
    pub bold: bool,
    pub accent: bool,
    pub error: bool,
    pub warning: bool,
    pub code: bool,
    pub italic: bool,
}

impl StyledLine {
    pub fn plain(text: impl Into<String>) -> StyledLine {
        StyledLine {
            text: text.into(),
            style: LineStyle::default(),
        }
    }

    pub fn styled(text: impl Into<String>, style: LineStyle) -> StyledLine {
        StyledLine {
            text: text.into(),
            style,
        }
    }

    /// Apply SGR styling to an already laid-out screen row.
    fn render(&self) -> String {
        let mut out = String::new();
        if self.style.dim {
            out.push_str(&style::Attribute::Dim.to_string());
        }
        if self.style.bold {
            out.push_str(&style::Attribute::Bold.to_string());
        }
        if self.style.italic {
            out.push_str(&style::Attribute::Italic.to_string());
        }
        if self.style.accent {
            out.push_str(&style::SetForegroundColor(style::Color::DarkCyan).to_string());
        } else if self.style.error {
            out.push_str(&style::SetForegroundColor(style::Color::Red).to_string());
        } else if self.style.warning {
            out.push_str(&style::SetForegroundColor(style::Color::Yellow).to_string());
        } else if self.style.code {
            out.push_str(&style::SetForegroundColor(style::Color::Grey).to_string());
        }
        out.push_str(&self.text);
        if self.style.dim || self.style.bold || self.style.italic {
            out.push_str(&style::Attribute::Reset.to_string());
        }
        if self.style.accent || self.style.error || self.style.warning || self.style.code {
            out.push_str(&style::SetForegroundColor(style::Color::Reset).to_string());
        }
        out
    }
}

/// A raw-mode terminal that owns the live region at the bottom of the
/// scrollback.
pub struct Terminal {
    region: region::Region,
}

impl Terminal {
    pub fn new() -> io::Result<Terminal> {
        terminal::enable_raw_mode()?;
        // Construct the guard before setup so errors still restore raw mode.
        let terminal = Terminal {
            region: region::Region::default(),
        };
        execute!(io::stdout(), EnableBracketedPaste)?;
        Ok(terminal)
    }

    pub fn size(&self) -> (usize, usize) {
        terminal::size()
            .map(|(columns, rows)| (columns.max(1) as usize, rows.max(1) as usize))
            .unwrap_or((80, 24))
    }

    /// Commit finished content. The caller repaints the live region next.
    pub fn push_lines(&mut self, lines: &[StyledLine]) -> io::Result<usize> {
        self.region
            .push(&mut io::stdout().lock(), lines, self.size().0)?;
        Ok(lines.len())
    }

    /// Replace the live region, bounded by physical screen rows.
    pub fn render_region(&mut self, lines: &[StyledLine]) -> io::Result<()> {
        self.region
            .render(&mut io::stdout().lock(), lines, self.size())
    }

    /// Place the cursor relative to the region's top, not its last position.
    pub fn cursor_in_region(&mut self, row: u16, column: u16) -> io::Result<()> {
        self.region.cursor(
            &mut io::stdout().lock(),
            row as usize,
            column as usize,
            self.size().0,
        )
    }

    pub fn clear(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        self.region.clear(&mut stdout)?;
        execute!(
            stdout,
            terminal::Clear(terminal::ClearType::All),
            cursor::MoveTo(0, 0)
        )
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), cursor::Hide)
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), cursor::Show)
    }

    /// Drain pending terminal events, reporting the interesting ones.
    pub fn pump(&mut self) -> io::Result<Vec<TermEvent>> {
        let mut events = Vec::new();
        while event::poll(Duration::from_millis(0))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    events.push(TermEvent::Key(key))
                }
                Event::Paste(text) => events.push(TermEvent::Paste(text)),
                Event::Resize(..) => events.push(TermEvent::Resize),
                _ => {}
            }
        }
        Ok(events)
    }

    /// Wait for one terminal event without draining or losing later events.
    pub fn wait_event(&mut self, timeout: Duration) -> io::Result<Option<TermEvent>> {
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    return Ok(Some(TermEvent::Key(key)));
                }
                Event::Key(_) => {}
                Event::Paste(text) => return Ok(Some(TermEvent::Paste(text))),
                Event::Resize(..) => return Ok(Some(TermEvent::Resize)),
                _ => {}
            }
        }
        Ok(None)
    }

    /// Ring the terminal bell (ignored on terminals without one).
    pub fn beep(&mut self) {
        let _ = io::stdout().write_all(b"\x07");
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.region.finish(&mut io::stdout().lock());
        let _ = execute!(
            io::stdout(),
            cursor::Show,
            style::SetAttribute(style::Attribute::Reset),
            style::ResetColor,
            DisableBracketedPaste,
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// A pumped terminal event, with paste decoded.
#[derive(Debug, Clone)]
pub enum TermEvent {
    Key(KeyEvent),
    /// Bracketed paste payload.
    Paste(String),
    Resize,
}

/// Is this key the terminal's interrupt (Ctrl+C)?
pub fn is_interrupt(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
}

/// Is this a paste or a regular character?
fn is_printable(key: &KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Single-line-oriented input editor with history, kill ring basics, and
/// bracketed paste support.
pub struct LineEditor {
    pub text: String,
    /// Byte cursor position within `text`.
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    /// Ctrl+C on an empty line requested a quit.
    pub quit_requested: bool,
    /// Staged text while browsing history.
    staged: Option<String>,
}

impl LineEditor {
    pub fn new(history: Vec<String>) -> LineEditor {
        LineEditor {
            text: String::new(),
            cursor: 0,
            history,
            history_index: None,
            quit_requested: false,
            staged: None,
        }
    }

    /// Feed one key event; returns true when the user submitted the line.
    pub fn handle_key(&mut self, key: &KeyEvent, terminal: &mut Terminal) -> io::Result<bool> {
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert('\n');
            }
            KeyCode::Enter => return Ok(true),
            // Ctrl+J is the canonical newline key on some terminals.
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert('\n');
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.text.is_empty() {
                    self.quit_requested = true;
                    return Ok(true);
                }
                self.clear();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.text.truncate(self.cursor);
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_word_back();
            }
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                self.delete_word_back();
            }
            KeyCode::Char(ch) if is_printable(key) => self.insert(ch),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let boundary = prev_char_boundary(&self.text, self.cursor);
                    self.text.drain(boundary..self.cursor);
                    self.cursor = boundary;
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    let boundary = next_char_boundary(&self.text, self.cursor);
                    self.text.drain(self.cursor..boundary);
                }
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.text, self.cursor);
                }
            }
            KeyCode::Right => {
                if self.cursor < self.text.len() {
                    self.cursor = next_char_boundary(&self.text, self.cursor);
                }
            }
            KeyCode::Home | KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = 0;
            }
            KeyCode::End | KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.text.len();
            }
            KeyCode::Up
                if key.modifiers.is_empty() || key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.history_older();
                terminal.beep();
            }
            KeyCode::Down
                if key.modifiers.is_empty() || key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.history_newer();
            }
            _ => {}
        }
        Ok(false)
    }

    fn insert(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.history_index = None;
        self.staged = None;
    }

    /// Insert pasted text, retaining newlines but not terminal controls.
    pub fn insert_str(&mut self, text: &str) {
        let text = turnip_core::text::sanitize_str(text);
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.history_index = None;
        self.staged = None;
    }

    /// Lay out input and its cursor together, then keep the cursor in view.
    /// Unlike word wrapping, this preserves whitespace and exact positions.
    pub fn view(&self, width: usize, max_rows: usize) -> (Vec<StyledLine>, (usize, usize)) {
        let width = width.max(1);
        let mut lines = Vec::new();
        let mut position = (0, 0);
        let mut offset = 0;
        for (index, raw) in self.text.split('\n').enumerate() {
            let prefix = if index == 0 { PROMPT } else { "  " };
            let wrapped = region::wrap_line(&format!("{prefix}{raw}"), width);
            if (offset..=offset + raw.len()).contains(&self.cursor) {
                let before = format!("{prefix}{}", &raw[..self.cursor - offset]);
                let before = region::wrap_line(&before, width);
                let mut row = lines.len() + before.len() - 1;
                let mut column = turnip_core::text::str_width(before.last().unwrap());
                let next_width = raw[self.cursor - offset..]
                    .chars()
                    .next()
                    .and_then(turnip_core::text::char_width)
                    .unwrap_or(0)
                    .min(width);
                if column == width || column + next_width > width {
                    row += 1;
                    column = 0;
                }
                position = (row, column);
            }
            let first_row = index == 0;
            lines.extend(wrapped.into_iter().enumerate().map(|(row, text)| {
                // The prompt row carries the input's identity: accent, like
                // the echo a submitted turn leaves in the scrollback.
                if first_row && row == 0 {
                    StyledLine::styled(text, LineStyle {
                        accent: true,
                        ..Default::default()
                    })
                } else {
                    StyledLine::plain(text)
                }
            }));
            offset += raw.len() + 1;
        }
        while lines.len() <= position.0 {
            lines.push(StyledLine::plain(""));
        }
        let start = position.0.saturating_sub(max_rows.max(1) - 1);
        let visible = lines
            .into_iter()
            .skip(start)
            .take(max_rows.max(1))
            .collect();
        (visible, (position.0 - start, position.1))
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.history_index = None;
        self.staged = None;
        self.quit_requested = false;
    }

    /// Take the submitted text and reset the editor.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.history_index = None;
        self.staged = None;
        self.quit_requested = false;
        text
    }

    fn delete_word_back(&mut self) {
        let mut boundary = self.cursor;
        // Skip whitespace, then a word.
        while boundary > 0 && self.text[..boundary].ends_with(char::is_whitespace) {
            boundary = prev_char_boundary(&self.text, boundary);
        }
        while boundary > 0 {
            let previous = prev_char_boundary(&self.text, boundary);
            if self.text[previous..boundary]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
            {
                break;
            }
            boundary = previous;
        }
        self.text.drain(boundary..self.cursor);
        self.cursor = boundary;
    }

    fn history_older(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.staged = Some(self.text.clone());
            self.history_index = Some(self.history.len() - 1);
        } else if let Some(index) = self.history_index {
            if index > 0 {
                self.history_index = Some(index - 1);
            } else {
                return;
            }
        }
        self.text = self.history[self.history_index.unwrap()].clone();
        self.cursor = self.text.len();
    }

    fn history_newer(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.text = self.history[index + 1].clone();
        } else {
            self.history_index = None;
            self.text = self.staged.take().unwrap_or_default();
        }
        self.cursor = self.text.len();
    }
}

fn prev_char_boundary(text: &str, cursor: usize) -> usize {
    let mut boundary = cursor - 1;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn next_char_boundary(text: &str, cursor: usize) -> usize {
    let mut boundary = cursor + 1;
    while boundary < text.len() && !text.is_char_boundary(boundary) {
        boundary += 1;
    }
    boundary.min(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str, cursor: usize) -> LineEditor {
        let mut editor = LineEditor::new(Vec::new());
        editor.insert_str(text);
        editor.cursor = cursor;
        editor
    }

    fn text(lines: &[StyledLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn input_preserves_spaces_and_tracks_wrapped_cursor() {
        let editor = editor("ab  cdefgh", 8);
        let (lines, cursor) = editor.view(6, 10);
        assert_eq!(text(&lines), ["› ab  ", "cdefgh"]);
        assert_eq!(cursor, (1, 4));
    }

    #[test]
    fn exact_width_input_has_a_visible_cursor_on_the_next_row() {
        let editor = editor("abcd", 4);
        let (lines, cursor) = editor.view(6, 10);
        assert_eq!(text(&lines), ["› abcd", ""]);
        assert_eq!(cursor, (1, 0));
    }

    #[test]
    fn input_cursor_accounts_for_newlines_and_wide_characters() {
        let editor = editor("界x\n世界", "界x\n世".len());
        let (lines, cursor) = editor.view(6, 10);
        assert_eq!(text(&lines), ["› 界x", "  世界"]);
        assert_eq!(cursor, (1, 4));

        let editor = self::editor("abc界", 3);
        let (lines, cursor) = editor.view(6, 10);
        assert_eq!(text(&lines), ["› abc", "界"]);
        assert_eq!(cursor, (1, 0));
    }

    #[test]
    fn long_input_is_scrolled_to_the_cursor_not_always_the_end() {
        let mut editor = editor("one\ntwo\nthree\nfour", 3);
        let (lines, cursor) = editor.view(12, 2);
        assert_eq!(text(&lines), ["› one", "  two"]);
        assert_eq!(cursor, (0, 5));

        editor.cursor = editor.text.len();
        let (lines, cursor) = editor.view(12, 2);
        assert_eq!(text(&lines), ["  three", "  four"]);
        assert_eq!(cursor, (1, 6));
    }

    #[test]
    fn paste_strips_terminal_controls_and_expands_tabs() {
        let mut editor = LineEditor::new(Vec::new());
        editor.insert_str("a\x1b[2Jb\r\nc\td");
        assert_eq!(editor.text, "ab\nc    d");
        assert_eq!(editor.cursor, editor.text.len());
    }

    #[test]
    fn input_fits_even_a_one_cell_terminal() {
        let editor = editor("界", "界".len());
        let (lines, cursor) = editor.view(1, 1);
        assert_eq!(lines.len(), 1);
        assert_eq!(cursor, (0, 0));
        assert!(turnip_core::text::str_width(&lines[0].text) <= 1);
    }
}
