//! Terminal handling: raw mode, the scrollback/live-region rendering model,
//! key event pumping, and the input line editor.
//!
//! Like hax, turnip never takes over the screen. Completed content is
//! written into the terminal's native scrollback; only a bounded live region
//! (streaming tail, status line, input) is redrawn, and Ctrl+C stays
//! cooperative everywhere.

pub mod markdown;
pub mod picker;

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

    /// The line rendered with SGR escapes, wrapped to `width` columns.
    pub fn render(&self, width: usize) -> String {
        let mut out = String::new();
        let wrapped = turnip_core::text::wrap_line(&self.text, width.max(4));
        let mut first = true;
        for line in wrapped {
            if !first {
                out.push_str("\r\n");
            }
            first = false;
            if self.style.dim {
                out.push_str(&style::Attribute::Dim.to_string());
            }
            if self.style.bold {
                out.push_str(&style::Attribute::Bold.to_string());
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
            out.push_str(&line);
            if self.style.dim || self.style.bold {
                out.push_str(&style::Attribute::Reset.to_string());
            }
            if self.style.accent || self.style.error || self.style.warning || self.style.code {
                out.push_str(&style::SetForegroundColor(style::Color::Reset).to_string());
            }
        }
        out
    }
}

/// A raw-mode terminal that owns the live region at the bottom of the
/// scrollback.
pub struct Terminal {
    enabled: bool,
    /// Lines currently occupied by the live region (for redraw/erase).
    region_lines: usize,
    /// Row (from the region top) where the cursor currently sits, so the
    /// write invariant can be restored before the next region operation.
    cursor_row: u16,
    /// Events read but not yet consumed (e.g. pastes waiting behind keys).
    pending: std::collections::VecDeque<TermEvent>,
}

impl Terminal {
    pub fn new() -> io::Result<Terminal> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            terminal::Clear(terminal::ClearType::Purge),
            EnableBracketedPaste
        );
        Ok(Terminal {
            enabled: true,
            region_lines: 0,
            cursor_row: 0,
            pending: std::collections::VecDeque::new(),
        })
    }

    pub fn size(&self) -> (usize, usize) {
        terminal::size()
            .map(|(columns, rows)| (columns as usize, rows as usize))
            .unwrap_or((80, 24))
    }

    /// Write finished lines into the scrollback, above the live region.
    /// `lines` must contain no newlines. Returns the number written.
    ///
    /// The invariant everywhere is: **the cursor sits at the live region's
    /// top whenever the terminal is asked to write**. The caller may move it
    /// into the region with [`Terminal::cursor_in_region`]; this first walks
    /// back up, then writes the lines from the region top, so the region
    /// shifts down intact and the pushed lines land just above it.
    pub fn push_lines(&mut self, lines: &[StyledLine]) -> io::Result<usize> {
        if lines.is_empty() {
            return Ok(0);
        }
        self.restore_to_top()?;
        let (width, _) = self.size();
        let mut stdout = io::stdout();
        for line in lines {
            stdout.write_all(line.render(width).as_bytes())?;
            stdout.write_all(b"\r\n")?;
        }
        stdout.flush()?;
        Ok(lines.len())
    }

    /// Replace the live region with `lines` (no embedded newlines; wrapping
    /// is applied per line). Leaves the cursor at the region's top, keeping
    /// the write invariant for the next call.
    pub fn render_region(&mut self, lines: &[StyledLine]) -> io::Result<()> {
        self.restore_to_top()?;
        let old_lines = self.region_lines;
        let mut stdout = io::stdout();
        // Erase the previous region, walking its rows from the top.
        for index in 0..old_lines {
            if index > 0 {
                stdout.write_all(b"\r\n")?;
            }
            execute!(stdout, cursor::MoveToColumn(0), terminal::Clear(terminal::ClearType::UntilNewLine))?;
        }
        // Walk back to the region top.
        if old_lines > 1 {
            execute!(stdout, cursor::MoveUp(old_lines as u16 - 1))?;
        }

        let (width, _) = self.size();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                stdout.write_all(b"\r\n")?;
            }
            execute!(stdout, cursor::MoveToColumn(0))?;
            stdout.write_all(line.render(width).as_bytes())?;
        }

        // Erase rows of the previous region the new one does not cover.
        let leftover = old_lines.saturating_sub(lines.len());
        for _ in 0..leftover {
            stdout.write_all(b"\r\n")?;
            execute!(stdout, cursor::MoveToColumn(0), terminal::Clear(terminal::ClearType::UntilNewLine))?;
        }
        if leftover > 0 {
            execute!(stdout, cursor::MoveUp(leftover as u16))?;
        }

        self.region_lines = lines.len().max(1);
        self.cursor_row = 0;
        stdout.flush()?;
        // Park the cursor at the region top so the next write starts there.
        if self.region_lines > 1 {
            execute!(io::stdout(), cursor::MoveUp(self.region_lines as u16 - 1))?;
        }
        Ok(())
    }

    /// Place the cursor at (row, column) within the current region, where
    /// row 0 is the region's top line. The terminal remembers the row so
    /// the next region write can restore the invariant.
    pub fn cursor_in_region(&mut self, row_from_top: u16, column: u16) -> io::Result<()> {
        let mut stdout = io::stdout();
        execute!(stdout, cursor::MoveDown(row_from_top), cursor::MoveToColumn(column))?;
        self.cursor_row = row_from_top;
        stdout.flush()
    }

    /// Walk the cursor back to the live region's top.
    fn restore_to_top(&mut self) -> io::Result<()> {
        if self.cursor_row > 0 {
            execute!(io::stdout(), cursor::MoveUp(self.cursor_row))?;
            self.cursor_row = 0;
        }
        Ok(())
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
        events.extend(self.pending.drain(..));
        Ok(events)
    }

    /// Wait for the next terminal event, draining any already-pending ones
    /// first (so pastes read behind keys are not lost).
    pub fn wait_event(&mut self, timeout: Duration) -> io::Result<Option<TermEvent>> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
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
        if self.enabled {
            let _ = self.restore_to_top();
            if self.region_lines > 1 {
                let _ = execute!(
                    io::stdout(),
                    cursor::MoveDown(self.region_lines as u16 - 1)
                );
            }
            let _ = io::stdout().write_all(b"\r\n");
            let _ = execute!(
                io::stdout(),
                cursor::Show,
                style::ResetColor,
                DisableBracketedPaste,
            );
            let _ = terminal::disable_raw_mode();
        }
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
    !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
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
            KeyCode::Up if key.modifiers.is_empty() || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.history_older();
                terminal.beep();
            }
            KeyCode::Down if key.modifiers.is_empty() || key.modifiers.contains(KeyModifiers::SHIFT) => {
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

    /// Insert pasted text verbatim (newlines included).
    pub fn insert_str(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.history_index = None;
        self.staged = None;
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

