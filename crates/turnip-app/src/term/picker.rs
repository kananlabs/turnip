//! A small interactive list picker with substring filtering, used for
//! models, providers, efforts, and sessions. Runs as a modal overlay inside
//! the live region.

use std::io;

use crossterm::event::{KeyCode, KeyModifiers};

use super::{LineStyle, StyledLine, Terminal};

/// One selectable row.
pub struct PickerItem {
    /// The value returned on selection.
    pub id: String,
    /// Primary label shown.
    pub label: String,
    /// Optional secondary detail.
    pub detail: Option<String>,
}

/// Filter `items` by a case-insensitive substring over label + detail.
fn matches(item: &PickerItem, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let filter = filter.to_ascii_lowercase();
    item.label.to_ascii_lowercase().contains(&filter)
        || item
            .detail
            .as_deref()
            .is_some_and(|d| d.to_ascii_lowercase().contains(&filter))
}

/// Run the picker; returns the selected id, or None on Escape / empty list.
pub fn pick(
    terminal: &mut Terminal,
    title: &str,
    items: Vec<PickerItem>,
    initial_filter: &str,
) -> io::Result<Option<String>> {
    if items.is_empty() {
        return Ok(None);
    }
    let mut filter = initial_filter.to_string();
    let mut cursor = 0usize;

    loop {
        let visible: Vec<&PickerItem> = items.iter().filter(|i| matches(i, &filter)).collect();
        if cursor >= visible.len() {
            cursor = visible.len().saturating_sub(1);
        }
        let (width, height) = terminal.size();
        // Leave at least one item row on small screens when possible.
        let show_title = height >= 4;
        let show_count = height >= 3;
        let rows = height.saturating_sub(1 + usize::from(show_title) + usize::from(show_count));

        let mut lines = Vec::new();
        if show_title {
            lines.push(StyledLine::styled(
                turnip_core::text::truncate_columns(
                    &format!("{title} (Enter: select, Esc: cancel)"),
                    width,
                ),
                LineStyle {
                    bold: true,
                    ..Default::default()
                },
            ));
        }
        let filter_row = lines.len();
        // Show the end of a long filter and keep its cursor on screen.
        let mut filter_tail = String::new();
        let mut columns = 0;
        for ch in filter.chars().rev() {
            let cells = turnip_core::text::char_width(ch).unwrap_or(0);
            if columns + cells > width.saturating_sub(2) {
                break;
            }
            filter_tail.insert(0, ch);
            columns += cells;
        }
        lines.push(StyledLine::styled(
            turnip_core::text::truncate_columns(&format!("/{filter_tail}"), width),
            LineStyle {
                accent: true,
                ..Default::default()
            },
        ));
        let start = cursor
            .saturating_sub(rows / 2)
            .min(visible.len().saturating_sub(rows));
        for (index, item) in visible.iter().enumerate().skip(start).take(rows) {
            let marker = if index == cursor { "› " } else { "  " };
            let mut text = format!("{marker}{}", item.label);
            if let Some(detail) = &item.detail {
                text.push_str(&format!("  {detail}"));
            }
            let style = LineStyle {
                accent: index == cursor,
                dim: index != cursor,
                ..Default::default()
            };
            let text = turnip_core::text::sanitize_str(&text).replace('\n', " ");
            lines.push(StyledLine::styled(
                turnip_core::text::truncate_columns(&text, width),
                style,
            ));
        }
        if visible.is_empty() && rows > 0 {
            lines.push(StyledLine::styled(
                turnip_core::text::truncate_columns("  no matches", width),
                LineStyle {
                    dim: true,
                    ..Default::default()
                },
            ));
        }
        let count_note = if visible.len() == items.len() {
            format!("{} items", items.len())
        } else {
            format!("{}/{}", visible.len(), items.len())
        };
        if show_count {
            lines.push(StyledLine::styled(
                turnip_core::text::truncate_columns(&count_note, width),
                LineStyle {
                    dim: true,
                    ..Default::default()
                },
            ));
        }
        terminal.render_region(&lines)?;
        terminal.cursor_in_region(filter_row as u16, (1 + columns) as u16)?;

        // Input loop.
        let event = loop {
            match terminal.wait_event(std::time::Duration::from_millis(200))? {
                Some(crate::term::TermEvent::Key(key)) => break Some(key),
                Some(crate::term::TermEvent::Paste(text)) => {
                    filter.push_str(&turnip_core::text::sanitize_str(&text).replace('\n', " "));
                    cursor = 0;
                    break None;
                }
                Some(crate::term::TermEvent::Resize) => break None,
                None => continue,
            }
        };
        match event {
            None => continue,
            Some(key) => match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Enter => {
                    if let Some(item) = visible.get(cursor) {
                        return Ok(Some(item.id.clone()));
                    }
                }
                KeyCode::Up => {
                    if cursor > 0 {
                        cursor -= 1;
                    } else {
                        terminal.beep();
                    }
                }
                KeyCode::Down => {
                    if cursor + 1 < visible.len() {
                        cursor += 1;
                    } else {
                        terminal.beep();
                    }
                }
                KeyCode::Backspace => {
                    filter.pop();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    filter.push(ch);
                }
                _ => {}
            },
        }
    }
}
