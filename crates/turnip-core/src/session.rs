//! Append-only conversation persistence.
//!
//! Each session is a JSONL file under the working directory's bucket in the
//! platform state tree. The first line is a header; subsequent lines are
//! items or control records: complete provider/model/effort selections,
//! `/undo` cuts, and completed user-turn timings. Nothing is ever rewritten,
//! so a reader following the file sees every record once and applies undo
//! records itself.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::item::{items_user_turn_cut, Item};
use crate::{CoreError, Result};

/// FNV-1a over the cwd: the bucket name grouping one working directory's
/// sessions. Full 64-bit hex keeps distinct projects from colliding.
pub fn bucket_for(cwd: &Path) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in cwd.to_string_lossy().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Directory holding one cwd's session files.
pub fn bucket_dir(cwd: &Path) -> Option<PathBuf> {
    crate::config::state_dir().map(|base| base.join("sessions").join(bucket_for(cwd)))
}

/// Prompt history for a cwd: a sibling of its session files that listing and
/// pruning ignore.
pub fn prompt_history_path(cwd: &Path) -> Option<PathBuf> {
    bucket_dir(cwd).map(|dir| dir.join("history.txt"))
}

/// Identity and the effective selection recorded in a session header.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub cwd: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub model_label: Option<String>,
    pub effort: Option<String>,
}

/// The file's own view of a selection at a point in time.
#[derive(Debug, Clone, Default)]
struct Selection {
    provider: Option<String>,
    model: Option<String>,
    model_label: Option<String>,
    effort: Option<String>,
}

/// Everything a session file says about its conversation.
#[derive(Debug, Default)]
pub struct SessionLoaded {
    /// The file these records came from.
    pub path: PathBuf,
    pub items: Vec<Item>,
    pub retired: Vec<Item>,
    pub worked_ms: i64,
    pub last_user_turn_ms: i64,
    pub meta: SessionMeta,
}

/// One row of the session picker.
#[derive(Debug, Clone, Default)]
pub struct SessionEntry {
    pub path: PathBuf,
    pub id: String,
    pub mtime_secs: i64,
    pub label: SessionLabel,
}

/// What a picker row can say about a session without replaying it.
#[derive(Debug, Clone, Default)]
pub struct SessionLabel {
    pub prompt: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// A writer for one session file. Created lazily materializing: the header
/// is written on the first append, so a session that records nothing leaves
/// no file behind.
pub struct SessionLog {
    path: PathBuf,
    file: Option<File>,
    /// Number of items already represented in the file.
    written: usize,
    /// Selection staged to be written before the next item.
    pending: Option<Selection>,
    pub meta: SessionMeta,
    header_written: bool,
}

impl SessionLog {
    /// Prepare a fresh session for the current directory. Returns None when
    /// the state path cannot be resolved.
    pub fn open(
        provider: &str,
        model: Option<&str>,
        model_label: Option<&str>,
        effort: Option<&str>,
    ) -> Option<SessionLog> {
        let cwd = std::env::current_dir().ok()?;
        let dir = bucket_dir(&cwd)?;
        let id = uuid::Uuid::new_v4().to_string();
        let path = dir.join(format!("{}-{id}.jsonl", file_timestamp_now()));
        let meta = SessionMeta {
            id,
            cwd: cwd.to_string_lossy().into_owned(),
            provider: Some(provider.to_string()),
            model: model.map(String::from),
            model_label: model_label.map(String::from),
            effort: effort.map(String::from),
        };
        Some(SessionLog {
            path,
            file: None,
            written: 0,
            pending: None,
            meta,
            header_written: false,
        })
    }

    /// Reopen `path` for appending; `loaded_item_count` items are already
    /// represented in the file. A staged selection record is written before
    /// the next item when the run's selection differs from the file's.
    pub fn resume(
        path: &Path,
        provider: &str,
        model: Option<&str>,
        model_label: Option<&str>,
        effort: Option<&str>,
        loaded_item_count: usize,
    ) -> Option<SessionLog> {
        let file = OpenOptions::new().append(true).open(path).ok()?;
        let meta = SessionMeta {
            id: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            cwd: String::new(),
            provider: Some(provider.to_string()),
            model: model.map(String::from),
            model_label: model_label.map(String::from),
            effort: effort.map(String::from),
        };
        Some(SessionLog {
            path: path.to_path_buf(),
            file: Some(file),
            written: loaded_item_count,
            pending: None,
            meta,
            header_written: true,
        })
    }

    /// Write the header before any item exists, so the id is a resume handle
    /// from the start. A no-op once materialized.
    pub fn begin(&mut self) {
        self.materialize();
    }

    fn materialize(&mut self) {
        if self.header_written {
            return;
        }
        self.header_written = true;
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if self.file.is_none() {
            self.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok();
        }
        if let Some(file) = &mut self.file {
            let header = json!({
                "type": "session",
                "id": self.meta.id,
                "timestamp": file_timestamp_now(),
                "cwd": self.meta.cwd,
                "provider": self.meta.provider,
                "model": self.meta.model,
                "model_label": self.meta.model_label,
                "effort": self.meta.effort,
            });
            let _ = writeln!(file, "{}", header);
        }
    }

    /// Append items not previously written.
    pub fn append(&mut self, items: &[Item]) {
        if items.len() <= self.written {
            return;
        }
        self.materialize();
        if let Some(pending) = self.pending.take() {
            self.write_selection(&pending);
        }
        if let Some(file) = &mut self.file {
            for item in &items[self.written..] {
                let _ = writeln!(file, "{}", serde_json::to_string(item).unwrap_or_default());
            }
        }
        self.written = items.len();
    }

    /// Stage a selection change; it is written immediately before the next
    /// appended item, so an unused selection does not alter a resumed
    /// conversation.
    pub fn set_meta(
        &mut self,
        provider: Option<&str>,
        model: Option<&str>,
        model_label: Option<&str>,
        effort: Option<&str>,
    ) {
        self.pending = Some(Selection {
            provider: provider.map(String::from),
            model: model.map(String::from),
            model_label: model_label.map(String::from),
            effort: effort.map(String::from),
        });
    }

    /// Drop a staged, unwritten selection so the next append does not commit
    /// it (for a log about to close with nothing after the change).
    pub fn discard_selection(&mut self) {
        self.pending = None;
    }

    fn write_selection(&mut self, selection: &Selection) {
        if let Some(file) = &mut self.file {
            let record = json!({
                "type": "selection",
                "provider": selection.provider,
                "model": selection.model,
                "model_label": selection.model_label,
                "effort": selection.effort,
            });
            let _ = writeln!(file, "{}", record);
        }
    }

    /// Record an `/undo` cut: the conversation keeps its first
    /// `keep_user_turns` typed prompts; `new_item_count` becomes the
    /// in-memory high-water mark.
    pub fn undo(&mut self, keep_user_turns: usize, new_item_count: usize) -> Result<()> {
        if !self.header_written {
            self.written = new_item_count;
            return Ok(());
        }
        if let Some(file) = &mut self.file {
            let record = json!({
                "type": "undo",
                "keep_user_turns": keep_user_turns,
                "new_item_count": new_item_count,
            });
            writeln!(file, "{}", record)?;
        }
        self.written = new_item_count;
        Ok(())
    }

    /// Record the wall time of a completed user turn.
    pub fn user_turn(&mut self, elapsed_ms: i64) {
        if let Some(file) = &mut self.file {
            let record = json!({ "type": "user_turn", "elapsed_ms": elapsed_ms });
            let _ = writeln!(file, "{}", record);
        }
    }

    /// Close the current file and prepare a lazily materialized session with
    /// a fresh identity.
    pub fn reset(&mut self, provider: Option<&str>, model: Option<&str>, effort: Option<&str>) {
        self.file = None;
        self.header_written = false;
        self.written = 0;
        self.pending = None;
        self.meta.id = uuid::Uuid::new_v4().to_string();
        if let Some(cwd) = std::env::current_dir().ok() {
            self.path = bucket_dir(&cwd)
                .map(|dir| dir.join(format!("{}-{}.jsonl", file_timestamp_now(), self.meta.id)))
                .unwrap_or_else(|| self.path.clone());
        }
        self.meta.provider = provider.map(String::from);
        self.meta.model = model.map(String::from);
        self.meta.effort = effort.map(String::from);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn materialized(&self) -> bool {
        self.header_written
    }

    pub fn flush(&mut self) {
        if let Some(file) = &mut self.file {
            let _ = file.flush();
        }
    }
}

/// Load the whole file: items, retired items, timing, and metadata. Invalid
/// JSON lines are skipped, and trailing incomplete tool calls are retired
/// from the live conversation.
pub fn load_all(path: &Path) -> Result<SessionLoaded> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut loaded = SessionLoaded {
        path: path.to_path_buf(),
        ..Default::default()
    };
    let mut wrote_meta = false;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let record_type = value.get("type").and_then(Value::as_str).unwrap_or_default();
        match record_type {
            "session" => {
                loaded.meta = SessionMeta {
                    id: value
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    cwd: value
                        .get("cwd")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    provider: value.get("provider").and_then(Value::as_str).map(String::from),
                    model: value.get("model").and_then(Value::as_str).map(String::from),
                    model_label: value
                        .get("model_label")
                        .and_then(Value::as_str)
                        .map(String::from),
                    effort: value.get("effort").and_then(Value::as_str).map(String::from),
                };
                wrote_meta = true;
            }
            "selection" => {
                // A selection record replaces the header's.
                if let Some(provider) = value.get("provider").and_then(Value::as_str) {
                    loaded.meta.provider = Some(provider.into());
                }
                loaded.meta.model = value
                    .get("model")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| loaded.meta.model.clone());
                loaded.meta.model_label = value
                    .get("model_label")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| loaded.meta.model_label.clone());
                loaded.meta.effort = value
                    .get("effort")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| loaded.meta.effort.clone());
            }
            "undo" => {
                let keep = value
                    .get("keep_user_turns")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let cut = items_user_turn_cut(&loaded.items, keep);
                loaded.retired.extend(loaded.items.drain(cut..));
                loaded.last_user_turn_ms = -1;
            }
            "user_turn" => {
                let elapsed = value.get("elapsed_ms").and_then(Value::as_i64).unwrap_or(0);
                loaded.worked_ms += elapsed;
                loaded.last_user_turn_ms = elapsed;
            }
            _ => match serde_json::from_value::<Item>(value) {
                Ok(item) => loaded.items.push(item),
                Err(_) => continue,
            },
        }
    }
    if !wrote_meta {
        return Err(CoreError::message(format!(
            "{}: not a session file (no header)",
            path.display()
        )));
    }
    retire_incomplete_tail(&mut loaded.items, &mut loaded.retired);
    Ok(loaded)
}

/// A trailing ToolCall without its ToolResult cannot be resumed into a valid
/// request; move it (and the reasoning directly feeding it) to retired.
fn retire_incomplete_tail(items: &mut Vec<Item>, retired: &mut Vec<Item>) {
    let has_result = |items: &[Item], id: &str| {
        items.iter().any(|i| matches!(i, Item::ToolResult { call_id, .. } if call_id == id))
    };
    while matches!(items.last(), Some(Item::ToolCall { call_id, .. }) if !has_result(items, call_id))
    {
        if let Some(item) = items.pop() {
            retired.push(item);
        }
    }
}

/// Write `items` into a sibling session with a fresh identity and a
/// `forked_from` header field, continuing from `selection`. Returns the new
/// path.
pub fn fork_file(
    source_path: &Path,
    items: &[Item],
    selection: &SessionMeta,
) -> Result<PathBuf> {
    let Some(dir) = source_path.parent() else {
        return Err(CoreError::message("session file has no directory"));
    };
    let id = uuid::Uuid::new_v4().to_string();
    let path = dir.join(format!("{}-{id}.jsonl", file_timestamp_now()));
    let mut file = File::create(&path)?;
    let header = json!({
        "type": "session",
        "id": id,
        "timestamp": file_timestamp_now(),
        "cwd": selection.cwd,
        "forked_from": source_path.file_stem().map(|s| s.to_string_lossy().into_owned()),
        "provider": selection.provider,
        "model": selection.model,
        "model_label": selection.model_label,
        "effort": selection.effort,
    });
    writeln!(file, "{}", header)?;
    for item in items {
        writeln!(file, "{}", serde_json::to_string(item)?)?;
    }
    Ok(path)
}

/// List one cwd's session files, newest first.
pub fn list(cwd: &Path) -> Vec<SessionEntry> {
    let Some(dir) = bucket_dir(cwd) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return entries;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .unwrap_or_default();
        entries.push(SessionEntry {
            id: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path,
            mtime_secs: mtime.as_secs() as i64,
            label: SessionLabel::default(),
        });
    }
    entries.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
    entries
}

/// Populate a picker label from a bounded file prefix: the first typed prompt
/// and the effective selection, without replaying the file.
pub fn label_read(entry: &mut SessionEntry, max_cells: usize) {
    const PREFIX: u64 = 32 * 1024;
    let Ok(mut file) = File::open(&entry.path) else {
        return;
    };
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    use std::io::Read;
    loop {
        match file.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                if bytes.len() as u64 >= PREFIX {
                    break;
                }
            }
        }
    }
    let text = crate::text::sanitize_utf8(&bytes);
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str).unwrap_or_default() {
            "session" | "selection" => {
                entry.label.provider = value
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| entry.label.provider.clone());
                entry.label.model = value
                    .get("model_label")
                    .or_else(|| value.get("model"))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| entry.label.model.clone());
                entry.label.effort = value
                    .get("effort")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| entry.label.effort.clone());
            }
            "user" => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    let summary = crate::text::prompt_summary(text, max_cells);
                    if entry.label.prompt.is_none() && !summary.is_empty() {
                        entry.label.prompt = Some(summary);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Whether `path` has turnip's timestamp-and-UUID session filename.
pub fn path_is_standard(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // 16 chars of timestamp, '-', 36 chars of UUID, ".jsonl".
    name.len() == 16 + 1 + 36 + 6
        && name.ends_with(".jsonl")
        && name.as_bytes()[16] == b'-'
        && name[..16].chars().all(|c| c.is_ascii_alphanumeric())
}

/// Delete this cwd's session files older than `retention_days`; 0 disables.
pub fn prune_expired(cwd: &Path, retention_days: i64) {
    if retention_days <= 0 {
        return;
    }
    let Some(dir) = bucket_dir(cwd) else {
        return;
    };
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(retention_days as u64 * 86_400);
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if let Ok(modified) = metadata.modified() {
            if modified < cutoff {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// UTC timestamp for filenames: `YYYYmmddTHHMMSSZ`.
pub fn file_timestamp_now() -> String {
    timestamp_from(std::time::SystemTime::now())
}

fn timestamp_from(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        tod / 3600,
        (tod / 60) % 60,
        tod % 60
    )
}

/// Human-friendly local-ish UTC time for pickers: `YYYY-mm-dd HH:MM`.
pub fn format_mtime(secs: i64) -> String {
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        tod / 3600,
        (tod / 60) % 60
    )
}

/// Readable "now" for prompt headers: `YYYY-mm-dd`.
pub fn format_timestamp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since the Unix epoch to (year, month, day) — Howard Hinnant's
/// civil-from-days algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// Append a prompt to the cwd's history file and return the recent tail.
pub fn history_append(cwd: &Path, prompt: &str) -> Vec<String> {
    let Some(path) = prompt_history_path(cwd) else {
        return Vec::new();
    };
    let single_line: String = prompt.replace('\n', " ");
    if !single_line.trim().is_empty() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(file, "{single_line}");
        }
    }
    history_read(cwd)
}

/// Recent prompt history for the cwd, oldest first, deduplicated.
pub fn history_read(cwd: &Path) -> Vec<String> {
    let Some(path) = prompt_history_path(cwd) else {
        return Vec::new();
    };
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut history = Vec::new();
    for line in BufReader::new(file).lines().map_while(|line| line.ok()) {
        if line.trim().is_empty() {
            continue;
        }
        if history.last().is_none_or(|prev: &String| prev != &line) {
            history.push(line);
        }
    }
    history
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 2024-01-01
    }

    #[test]
    fn file_timestamp_shape() {
        let stamp = file_timestamp_now();
        assert_eq!(stamp.len(), 16);
        assert!(stamp.ends_with('Z'));
        assert!(stamp.starts_with("20"));
    }

    #[test]
    fn session_roundtrip_with_undo() {
        let dir = std::env::temp_dir().join(format!("turnip-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("20250101T000000Z-00000000-0000-0000-0000-000000000000.jsonl");
        let mut log = SessionLog::open("openai", Some("gpt"), Some("GPT"), Some("high")).unwrap();
        log.path = path.clone();
        log.meta.cwd = dir.to_string_lossy().into_owned();
        log.begin();
        log.append(&[
            Item::Boundary,
            Item::user("first"),
            Item::assistant("one"),
            Item::Boundary,
            Item::user("second"),
            Item::assistant("two"),
        ]);
        log.undo(1, 3).unwrap();
        log.user_turn(1500);
        log.flush();

        let loaded = load_all(&path).unwrap();
        assert_eq!(loaded.items.len(), 3);
        assert_eq!(loaded.retired.len(), 3);
        assert_eq!(loaded.worked_ms, 1500);
        assert_eq!(loaded.meta.provider.as_deref(), Some("openai"));
        assert_eq!(loaded.meta.model.as_deref(), Some("gpt"));
        assert!(matches!(&loaded.items[1], Item::User { text, .. } if text == "first"));

        // A resume continues after the recorded tail.
        let mut resumed =
            SessionLog::resume(&path, "openai", Some("gpt"), Some("GPT"), Some("high"), 3).unwrap();
        resumed.append(&[
            Item::Boundary,
            Item::user("first"),
            Item::assistant("one"),
            Item::Boundary,
            Item::user("third"),
            Item::assistant("three"),
        ]);
        resumed.flush();
        let loaded = load_all(&path).unwrap();
        // Kept prefix (3) plus the boundary and the new turn's items (3).
        assert_eq!(loaded.items.len(), 6);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incomplete_tool_call_retired() {
        let mut items = vec![
            Item::user("go"),
            Item::ToolCall {
                call_id: "c1".into(),
                tool_name: "bash".into(),
                arguments: "{}".into(),
            },
            Item::tool_result("c1", "ok"),
            Item::ToolCall {
                call_id: "c2".into(),
                tool_name: "read".into(),
                arguments: "{}".into(),
            },
        ];
        let mut retired = Vec::new();
        retire_incomplete_tail(&mut items, &mut retired);
        assert_eq!(items.len(), 3);
        assert_eq!(retired.len(), 1);
    }

    #[test]
    fn standard_path_detection() {
        assert!(path_is_standard(Path::new(
            "20250101T000000Z-00000000-0000-0000-0000-000000000000.jsonl"
        )));
        assert!(!path_is_standard(Path::new("notes.jsonl")));
    }
}
