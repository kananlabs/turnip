//! Process-wide configuration.
//!
//! Settings resolve in this order (first tier that defines a key wins):
//!
//! ```text
//! run override -> resumed conversation -> environment -> state.json -> config.json -> default
//! ```
//!
//! `config.json` holds user preferences; `state.json` holds machine-local
//! state (last selection, session bookkeeping) that a settings sync should not
//! clobber. Files live under the platform directories (`~/.config/turnip`,
//! `~/.local/state/turnip` on Linux; `%APPDATA%\turnip`, `%LOCALAPPDATA%\turnip`
//! on Windows). Nested keys are dotted paths (`bash.timeout`); the
//! `providers.<id>.*` namespace is free-form and read by the provider registry.
//!
//! This module has no global state: a [`Config`] is an owned value the
//! application threads through, which keeps library embedding (e.g. Python
//! bindings) straightforward.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::{CoreError, Result};

/// Where a resolved value came from, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Run,
    Conversation,
    Env,
    State,
    File,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Run => "run",
            Source::Conversation => "conversation",
            Source::Env => "env",
            Source::State => "state",
            Source::File => "config",
            Source::Default => "default",
        }
    }
}

/// The kind of a setting, which selects its value grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    String,
    Int,
    /// Bytes; binary k/m suffixes (k = 1024).
    Size,
    /// Token counts; decimal k/m suffixes (k = 1000).
    Tokens,
    /// Milliseconds; accepts ms/s/m/h suffixes (bare numbers mean seconds).
    Duration,
    /// on|off.
    Bool,
    /// auto|on|off.
    Tristate,
}

/// One registry row describing a setting.
#[derive(Debug)]
pub struct Setting {
    /// Canonical dotted key.
    pub key: &'static str,
    /// Environment variable (derived from the key with the TURNIP_ prefix).
    pub env: &'static str,
    pub default: Option<&'static str>,
    pub description: &'static str,
    /// '|'-separated choices for string-kind settings.
    pub choices: Option<&'static str>,
    pub kind: Kind,
    /// An empty string is meaningful.
    pub keep_empty: bool,
}

const fn setting(
    key: &'static str,
    env: &'static str,
    default: Option<&'static str>,
    description: &'static str,
    kind: Kind,
) -> Setting {
    Setting {
        key,
        env,
        default,
        description,
        choices: None,
        kind,
        keep_empty: false,
    }
}

/// The known settings, ordered for display. Unlisted dotted keys still
/// resolve (as strings) through every tier; the registry drives defaults,
/// validation, and `/help` output.
pub const SETTINGS: &[Setting] = &[
    setting(
        "provider",
        "TURNIP_PROVIDER",
        None,
        "Provider id to use; the default is auto-selected from configured providers.",
        Kind::String,
    ),
    setting(
        "model",
        "TURNIP_MODEL",
        None,
        "Model id; defaults to the provider's default model when it has one.",
        Kind::String,
    ),
    setting(
        "effort",
        "TURNIP_EFFORT",
        None,
        "Reasoning effort, when the model accepts one (e.g. low, medium, high).",
        Kind::String,
    ),
    Setting {
        key: "system_prompt",
        env: "TURNIP_SYSTEM_PROMPT",
        default: None,
        description: "Base system prompt; a leading '@' names a file to read it from.",
        choices: None,
        kind: Kind::String,
        keep_empty: true,
    },
    Setting {
        key: "system_prompt_append",
        env: "TURNIP_SYSTEM_PROMPT_APPEND",
        default: None,
        description: "Text appended after the built-in system prompt.",
        choices: None,
        kind: Kind::String,
        keep_empty: true,
    },
    setting(
        "no_agents_md",
        "TURNIP_NO_AGENTS_MD",
        Some("off"),
        "Skip reading AGENTS.md into the system prompt (on|off).",
        Kind::Bool,
    ),
    setting(
        "markdown",
        "TURNIP_MARKDOWN",
        Some("on"),
        "Render assistant messages as Markdown in the terminal (on|off).",
        Kind::Bool,
    ),
    setting(
        "show_reasoning",
        "TURNIP_SHOW_REASONING",
        Some("off"),
        "Show model reasoning while it streams (on|off).",
        Kind::Bool,
    ),
    setting(
        "sort_models",
        "TURNIP_SORT_MODELS",
        Some("auto"),
        "Version-aware model sort in the picker (auto|on|off).",
        Kind::Tristate,
    ),
    setting(
        "context_limit",
        "TURNIP_CONTEXT_LIMIT",
        None,
        "Override the model's context window, in tokens (e.g. 128000).",
        Kind::Tokens,
    ),
    setting(
        "max_turns",
        "TURNIP_MAX_TURNS",
        Some("0"),
        "Stop after this many tool round-trips in one user turn; 0 means unlimited.",
        Kind::Int,
    ),
    setting(
        "compact.auto",
        "TURNIP_COMPACT_AUTO",
        Some("on"),
        "Summarize the conversation automatically when it nears the context window (on|off).",
        Kind::Bool,
    ),
    setting(
        "compact.threshold",
        "TURNIP_COMPACT_THRESHOLD",
        Some("85"),
        "Auto-compaction trigger, as a percentage of the context window (1-100).",
        Kind::Int,
    ),
    setting(
        "no_session",
        "TURNIP_NO_SESSION",
        Some("auto"),
        "Session recording (on|off|auto — auto disables it only for local providers).",
        Kind::Tristate,
    ),
    setting(
        "session_retention_days",
        "TURNIP_SESSION_RETENTION_DAYS",
        Some("30"),
        "Delete session files for a working directory after this many days; 0 disables pruning.",
        Kind::Int,
    ),
    Setting {
        key: "transcript",
        env: "TURNIP_TRANSCRIPT",
        default: None,
        description: "Append a JSONL transcript of every record sent and received to this file.",
        choices: None,
        kind: Kind::String,
        keep_empty: true,
    },
    setting(
        "tool_output_cap",
        "TURNIP_TOOL_OUTPUT_CAP",
        Some("50k"),
        "Maximum tool output kept for the model, in bytes (head+tail truncation).",
        Kind::Size,
    ),
    setting(
        "bash.timeout",
        "TURNIP_BASH_TIMEOUT",
        Some("2m"),
        "Default bash tool timeout; the model may override per call.",
        Kind::Duration,
    ),
    setting(
        "bash.timeout_max",
        "TURNIP_BASH_TIMEOUT_MAX",
        Some("30m"),
        "Maximum bash tool timeout the model may request.",
        Kind::Duration,
    ),
    setting(
        "bash.shell",
        "TURNIP_BASH_SHELL",
        None,
        "Shell used by the bash tool (default: bash on Unix; on Windows bash if available, else PowerShell).",
        Kind::String,
    ),
    Setting {
        key: "providers.openai-compatible.base_url",
        env: "TURNIP_OPENAI_BASE_URL",
        default: None,
        description: "Base URL for the openai-compatible provider.",
        choices: None,
        kind: Kind::String,
        keep_empty: true,
    },
    Setting {
        key: "providers.openai-compatible.api_key",
        env: "TURNIP_OPENAI_API_KEY",
        default: None,
        description: "API key for the openai-compatible provider.",
        choices: None,
        kind: Kind::String,
        keep_empty: true,
    },
    Setting {
        key: "providers.anthropic-compatible.base_url",
        env: "TURNIP_ANTHROPIC_BASE_URL",
        default: None,
        description: "Base URL for the anthropic-compatible provider.",
        choices: None,
        kind: Kind::String,
        keep_empty: true,
    },
    Setting {
        key: "providers.anthropic-compatible.api_key",
        env: "TURNIP_ANTHROPIC_API_KEY",
        default: None,
        description: "API key for the anthropic-compatible provider.",
        choices: None,
        kind: Kind::String,
        keep_empty: true,
    },
    Setting {
        key: "providers.llamacpp.base_url",
        env: "TURNIP_LLAMACPP_BASE_URL",
        default: None,
        description: "Base URL for llama.cpp's server.",
        choices: None,
        kind: Kind::String,
        keep_empty: true,
    },
    Setting {
        key: "providers.ollama.base_url",
        env: "TURNIP_OLLAMA_BASE_URL",
        default: None,
        description: "Base URL for ollama's OpenAI-compatible endpoint.",
        choices: None,
        kind: Kind::String,
        keep_empty: true,
    },
    setting(
        "http.max_retries",
        "TURNIP_HTTP_MAX_RETRIES",
        Some("4"),
        "Retries per request for transient failures (429/5xx/network).",
        Kind::Int,
    ),
    setting(
        "http.retry_base",
        "TURNIP_HTTP_RETRY_BASE",
        Some("1s"),
        "Base for exponential retry backoff.",
        Kind::Duration,
    ),
    setting(
        "http.idle_timeout",
        "TURNIP_HTTP_IDLE_TIMEOUT",
        Some("10m"),
        "Give up a stream when no bytes arrive for this long.",
        Kind::Duration,
    ),
];

/// Find a registry row by key.
pub fn setting_find(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}

/// Parse positive byte counts with case-insensitive binary k/m suffixes
/// (k = 1024, m = 1024*1024); invalid input yields 0.
pub fn parse_size(input: &str) -> u64 {
    parse_count(input, 1024)
}

/// Parse positive token counts with decimal suffixes (k = 1000, m = 1000000).
pub fn parse_token_count(input: &str) -> i64 {
    parse_count(input, 1000) as i64
}

fn parse_count(input: &str, scale: u64) -> u64 {
    let lower = input.trim().to_ascii_lowercase();
    let (number, multiplier) = if let Some(n) = lower.strip_suffix("kb") {
        (n, scale)
    } else if let Some(n) = lower.strip_suffix("mb") {
        (n, scale * scale)
    } else if let Some(n) = lower.strip_suffix('k') {
        (n, scale)
    } else if let Some(n) = lower.strip_suffix('m') {
        (n, scale * scale)
    } else {
        (lower.as_str(), 1)
    };
    number
        .parse::<u64>()
        .unwrap_or(0)
        .saturating_mul(multiplier)
}

/// Parse a non-negative duration with an optional ms/s/m/h suffix and return
/// milliseconds. A missing suffix means seconds. Returns -1 for invalid input.
pub fn parse_duration_ms(input: &str) -> i64 {
    let input = input.trim();
    let (number, unit) = match input.as_bytes().last() {
        Some(b'm' | b'h' | b's') if input.len() > 1 => {
            let cut = if input.ends_with("ms") { 2 } else { 1 };
            input.split_at(input.len() - cut)
        }
        Some(b'0'..=b'9') => (input, ""),
        _ => return -1,
    };
    let value: i64 = match number.parse() {
        Ok(v) => v,
        Err(_) => return -1,
    };
    match unit {
        "" => value * 1000,
        "ms" => value,
        "s" => value * 1000,
        "m" => value * 60_000,
        "h" => value * 3_600_000,
        _ => -1,
    }
}

/// Parse a boolean setting value.
pub fn parse_bool(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Some(true),
        "off" | "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

/// Parse a tristate setting value: 1 on, 0 off, -1 auto/unset/invalid.
pub fn parse_tristate(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Some(true),
        "off" | "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

/// Directory holding turnip's user configuration files.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("turnip"))
}

/// Directory holding turnip's machine-local state (sessions, last selection).
pub fn state_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("turnip"))
}

fn lookup_nested<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    let mut current = map.get(key.split('.').next()?)?;
    for segment in key.split('.').skip(1) {
        match current {
            Value::Object(object) => current = object.get(segment)?,
            _ => return None,
        }
    }
    Some(current)
}

fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Owned, threaded-through configuration state.
#[derive(Debug, Clone)]
pub struct Config {
    run: BTreeMap<String, String>,
    conversation: BTreeMap<String, String>,
    file: Map<String, Value>,
    state: Map<String, Value>,
    file_path: PathBuf,
    state_path: PathBuf,
    /// Whether config.json parsed (writes refuse to replace an unparseable file).
    file_ok: bool,
    /// Warnings produced while loading, surfaced once at startup.
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let file_path = config_dir().unwrap_or_else(|| PathBuf::from(".")).join("config.json");
        let state_path = state_dir().unwrap_or_else(|| PathBuf::from(".")).join("state.json");
        Config {
            run: BTreeMap::new(),
            conversation: BTreeMap::new(),
            file: Map::new(),
            state: Map::new(),
            file_path,
            state_path,
            file_ok: true,
            warnings: Vec::new(),
        }
    }
}

impl Config {
    /// Load config.json and state.json from their platform paths. Missing
    /// files are fine; unusable files record a warning and are skipped.
    pub fn load() -> Config {
        let mut config = Config::default();
        let file_path = config_dir().map(|d| d.join("config.json"));
        let state_path = state_dir().map(|d| d.join("state.json"));
        if let Some(path) = &file_path {
            config.file_path = path.clone();
            match read_json_object(path) {
                Ok(map) => config.file = map,
                Err(warning) => {
                    config.file_ok = false;
                    config.warnings.push(warning);
                }
            }
        }
        if let Some(path) = &state_path {
            config.state_path = path.clone();
            if let Ok(map) = read_json_object(path) {
                config.state = map;
            }
        }
        config
    }

    /// Resolve a key through every tier, skipping values an empty string does
    /// not make meaningful for. Returns the value and its source.
    pub fn resolve(&self, key: &str) -> Option<(String, Source)> {
        if let Some(value) = self.run.get(key) {
            return Some((value.clone(), Source::Run));
        }
        if let Some(value) = self.conversation.get(key) {
            return Some((value.clone(), Source::Conversation));
        }
        let setting = setting_find(key);
        if let Some(env) = setting.map(|s| s.env) {
            if let Ok(value) = std::env::var(env) {
                let keep_empty = setting.map(|s| s.keep_empty).unwrap_or(false);
                if !value.is_empty() || keep_empty {
                    return Some((value, Source::Env));
                }
            }
        }
        let keep_empty = setting.map(|s| s.keep_empty).unwrap_or(false);
        if let Some(value) = lookup_nested(&self.state, key).and_then(scalar_string) {
            if !value.is_empty() || keep_empty {
                return Some((value, Source::State));
            }
        }
        if let Some(value) = lookup_nested(&self.file, key).and_then(scalar_string) {
            if !value.is_empty() || keep_empty {
                return Some((value, Source::File));
            }
        }
        if let Some(default) = setting.and_then(|s| s.default) {
            return Some((default.to_string(), Source::Default));
        }
        None
    }

    /// Resolve a key, skipping empty values entirely.
    pub fn str(&self, key: &str) -> Option<String> {
        let (value, _) = self.resolve(key)?;
        if value.is_empty() {
            return None;
        }
        Some(value)
    }

    /// Which tier answered, or `None` when nothing resolves.
    pub fn source(&self, key: &str) -> Option<Source> {
        self.resolve(key).map(|(_, source)| source)
    }

    /// Resolve without the run-override tier (used to detect what a resumed
    /// conversation restores versus what the run overrides).
    pub fn str_below_run(&self, key: &str) -> Option<String> {
        if let Some(value) = self.conversation.get(key) {
            if !value.is_empty() {
                return Some(value.clone());
            }
        }
        if let Some(setting) = setting_find(key) {
            if let Ok(value) = std::env::var(setting.env) {
                if !value.is_empty() || setting.keep_empty {
                    return Some(value);
                }
            }
        }
        let keep_empty = setting_find(key).map(|s| s.keep_empty).unwrap_or(false);
        for map in [&self.state, &self.file] {
            if let Some(value) = lookup_nested(map, key).and_then(scalar_string) {
                if !value.is_empty() || keep_empty {
                    return Some(value);
                }
            }
        }
        setting_find(key).and_then(|s| s.default).map(String::from)
    }

    pub fn bool_or(&self, key: &str, default: bool) -> bool {
        self.str(key).and_then(|v| parse_bool(&v)).unwrap_or(default)
    }

    pub fn tristate_or(&self, key: &str) -> Option<bool> {
        self.str(key).and_then(|v| parse_tristate(&v))
    }

    pub fn int_or(&self, key: &str, default: i64) -> i64 {
        self.str(key)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(default)
    }

    pub fn duration_ms_or(&self, key: &str, default_ms: i64) -> i64 {
        self.str(key)
            .and_then(|v| {
                let ms = parse_duration_ms(&v);
                (ms >= 0).then_some(ms)
            })
            .unwrap_or(default_ms)
    }

    pub fn size_or(&self, key: &str, default: u64) -> u64 {
        let parsed = self
            .str(key)
            .map(|v| parse_size(&v))
            .unwrap_or(0);
        if parsed > 0 { parsed } else { default }
    }

    pub fn tokens_or(&self, key: &str, default: i64) -> i64 {
        let parsed = self
            .str(key)
            .map(|v| parse_token_count(&v))
            .unwrap_or(0);
        if parsed > 0 { parsed } else { default }
    }

    /// A structured block from the state tier, then the file tier, verbatim.
    /// The first tier that defines the block wins; blocks are not merged.
    pub fn json(&self, key: &str) -> Option<&Value> {
        lookup_nested(&self.state, key).or_else(|| lookup_nested(&self.file, key))
    }

    /// Immediate member names at `key` across both file tiers.
    pub fn object_keys(&self, key: &str) -> Vec<String> {
        let mut keys = Vec::new();
        for map in [&self.state, &self.file] {
            if let Some(Value::Object(object)) = lookup_nested(map, key) {
                for member in object.keys() {
                    if !keys.contains(member) {
                        keys.push(member.clone());
                    }
                }
            }
        }
        keys
    }

    /// Set or clear a value in the run-override tier.
    pub fn set_run(&mut self, key: &str, value: Option<&str>) {
        set_tier(&mut self.run, key, value);
    }

    /// Set or clear a value in the conversation tier (restored from a session).
    pub fn set_conversation(&mut self, key: &str, value: Option<&str>) {
        set_tier(&mut self.conversation, key, value);
    }

    pub fn clear_conversation(&mut self) {
        self.conversation.clear();
    }

    /// Atomically persist a nested key in config.json (or remove it when
    /// `value` is `None`) and update the in-memory tier.
    pub fn persist(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        if !self.file_ok {
            return Err(CoreError::message(
                "config.json could not be parsed; refusing to overwrite it",
            ));
        }
        set_nested(&mut self.file, key, value);
        write_json(&self.file_path, &Value::Object(self.file.clone()))?;
        Ok(())
    }

    /// Atomically persist a nested key in state.json and update the tier.
    pub fn persist_state(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        set_nested(&mut self.state, key, value);
        write_json(&self.state_path, &Value::Object(self.state.clone()))?;
        Ok(())
    }

    /// Persist a provider-bound selection to state.json in one write, so the
    /// next run starts where the last left off. `None` keeps the stored value
    /// for the same provider; switching providers selects the new default.
    pub fn persist_selection(
        &mut self,
        provider: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<()> {
        let previous = self.str("provider").unwrap_or_default();
        if previous != provider {
            self.state
                .insert("provider".into(), Value::String(provider.to_string()));
            self.state.insert("model".into(), Value::Null);
            self.state.insert("effort".into(), Value::Null);
        } else {
            self.state
                .insert("provider".into(), Value::String(provider.to_string()));
        }
        if let Some(model) = model {
            self.state
                .insert("model".into(), Value::String(model.to_string()));
        }
        if let Some(effort) = effort {
            self.state
                .insert("effort".into(), Value::String(effort.to_string()));
        }
        write_json(&self.state_path, &Value::Object(self.state.clone()))?;
        Ok(())
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }
}

fn set_tier(tier: &mut BTreeMap<String, String>, key: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            tier.insert(key.to_string(), value.to_string());
        }
        None => {
            tier.remove(key);
        }
    }
}

/// Insert or remove a dotted key inside a nested JSON document.
fn set_nested(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    let mut segments: Vec<&str> = key.split('.').collect();
    let leaf = segments.pop().expect("key is nonempty");
    let mut node: &mut Map<String, Value> = map;
    for segment in &segments {
        let entry = node
            .entry(segment.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        node = entry.as_object_mut().expect("entry is object");
    }
    match value {
        Some(value) => {
            node.insert(leaf.to_string(), Value::String(value.to_string()));
        }
        None => {
            node.remove(leaf);
        }
    }
    // Prune objects the removal left empty, so a cleared subtree disappears.
    while let Some(last) = segments.pop() {
        let removed = if segments.is_empty() {
            let empty = map.get(last).and_then(|v| v.as_object()).is_some_and(Map::is_empty);
            if empty {
                map.remove(last);
            }
            empty
        } else {
            let Some(object) = node_ref(map, &segments).and_then(|v| v.as_object_mut()) else {
                break;
            };
            let empty = object
                .get(last)
                .and_then(|v| v.as_object())
                .is_some_and(Map::is_empty);
            if empty {
                object.remove(last);
            }
            empty
        };
        if !removed {
            break;
        }
    }
}

fn node_ref<'a>(map: &'a mut Map<String, Value>, segments: &[&str]) -> Option<&'a mut Value> {
    let mut current = map.get_mut(segments[0])?;
    for segment in &segments[1..] {
        current = current.as_object_mut()?.get_mut(*segment)?;
    }
    Some(current)
}

fn read_json_object(path: &Path) -> std::result::Result<Map<String, Value>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        // Missing files are the normal first-run state, not a warning.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Map::new());
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(format!("{}: root is not an object", path.display())),
    }
}

/// Best-effort atomic write: temp file in the same directory, then rename
/// (replacing the destination first on Windows, where rename refuses to).
fn write_json(path: &Path, value: &Value) -> Result<()> {
    let body = serde_json::to_string_pretty(value).map_err(|e| CoreError::Message(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, body.as_bytes())?;
    #[cfg(windows)]
    {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
    std::fs::rename(&temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration_ms("2m"), 120_000);
        assert_eq!(parse_duration_ms("500ms"), 500);
        assert_eq!(parse_duration_ms("10"), 10_000);
        assert_eq!(parse_duration_ms("24h"), 86_400_000);
        assert_eq!(parse_duration_ms("bogus"), -1);
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("50k"), 51_200);
        assert_eq!(parse_size("1m"), 1_048_576);
        assert_eq!(parse_size("4096"), 4096);
        assert_eq!(parse_size("x"), 0);
    }

    #[test]
    fn token_parsing() {
        assert_eq!(parse_token_count("128k"), 128_000);
        assert_eq!(parse_token_count("1m"), 1_000_000);
    }

    #[test]
    fn tier_order_and_defaults() {
        let mut config = Config::default();
        config.set_run("effort", Some("high"));
        assert_eq!(config.str("effort").as_deref(), Some("high"));
        assert_eq!(config.source("effort"), Some(Source::Run));
        // Unset keys with a default resolve at the default tier.
        assert_eq!(config.str("compact.threshold").as_deref(), Some("85"));
        config.set_run("effort", None);
        assert_eq!(config.source("effort"), None);
    }

    #[test]
    fn nested_persist_and_read() {
        let mut config = Config::default();
        set_nested(&mut config.file, "bash.timeout", Some("5m"));
        assert_eq!(
            lookup_nested(&config.file, "bash.timeout").and_then(scalar_string),
            Some("5m".to_string())
        );
        set_nested(&mut config.file, "bash.timeout", None);
        assert!(lookup_nested(&config.file, "bash").is_none());
    }
}
