//! The `edit` tool: exact-string replacement with uniqueness enforcement and
//! a display-only diff preview.

use serde_json::Value;

use super::{display_path, resolve_path, Tool, ToolEnv, ToolIo};
use crate::provider::{ToolDef, ToolParam};
use crate::text;

const EDIT_DESCRIPTION: &str = "Edit a file by replacing one exact string with another.\n\n\
    `old_string` must match the file's current contents exactly (including whitespace and \
    indentation) and be unique unless `replace_all` is set. Include a few surrounding lines \
    when uniqueness needs context.";

pub struct EditTool {
    pub env: ToolEnv,
}

impl Tool for EditTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "edit",
            description: EDIT_DESCRIPTION,
            params: &[
                ToolParam {
                    name: "path",
                    r#type: "string",
                    item_type: None,
                    description: "Path to the file.",
                    required: true,
                },
                ToolParam {
                    name: "old_string",
                    r#type: "string",
                    item_type: None,
                    description: "Exact text to find. Must be unique unless replace_all is set.",
                    required: true,
                },
                ToolParam {
                    name: "new_string",
                    r#type: "string",
                    item_type: None,
                    description: "Replacement text.",
                    required: true,
                },
                ToolParam {
                    name: "replace_all",
                    r#type: "boolean",
                    item_type: None,
                    description: "Replace every occurrence instead of requiring uniqueness.",
                    required: false,
                },
            ],
        }
    }

    fn run(&self, args: &Value, io: &mut ToolIo) -> String {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return "error: 'path' is required".into();
        };
        let Some(old) = args.get("old_string").and_then(Value::as_str) else {
            return "error: 'old_string' is required".into();
        };
        let Some(new) = args.get("new_string").and_then(Value::as_str) else {
            return "error: 'new_string' is required".into();
        };
        if old == new {
            return "error: 'old_string' and 'new_string' are identical".into();
        }
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let resolved = resolve_path(&self.env, path);
        let display = display_path(&self.env, &resolved);
        let Ok(previous) = std::fs::read(&resolved) else {
            return format!("error: cannot read {display}");
        };
        let previous = text::sanitize_utf8(&previous);

        let matches = previous.match_indices(old).count();
        if matches == 0 {
            return format!(
                "error: old_string not found in {display}. Read the file and check the exact text, \
                 including whitespace and indentation."
            );
        }
        if matches > 1 && !replace_all {
            return format!(
                "error: old_string appears {matches} times in {display}. Add surrounding lines to \
                 make it unique, or pass replace_all to replace every occurrence."
            );
        }

        let updated = if replace_all {
            previous.replace(old, new)
        } else {
            previous.replacen(old, new, 1)
        };
        if let Err(error) = std::fs::write(&resolved, &updated) {
            return format!("error: cannot write {display}: {error}");
        }
        let diff = text::unified_diff(&previous, &updated, 2);
        let (added, removed) = text::diff_counts(&diff);
        if !diff.is_empty() {
            io.note(&diff);
        }
        format!(
            "Edited {display}: {matches} replacement(s), {added} line(s) added, {removed} removed"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn fresh_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("turnip-test-edit-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tool(dir: &std::path::Path) -> EditTool {
        EditTool {
            env: ToolEnv {
                cwd: dir.to_path_buf(),
                config: Config::default(),
            },
        }
    }

    #[test]
    fn edits_unique_match() {
        let dir = fresh_dir("unique");
        std::fs::write(dir.join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let tool = tool(&dir);
        let mut io = ToolIo::quiet();
        let output = tool.run(
            &serde_json::json!({"path": "f.txt", "old_string": "beta", "new_string": "BETA"}),
            &mut io,
        );
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "alpha\nBETA\ngamma\n");
        assert!(output.starts_with("Edited f.txt"));
    }

    #[test]
    fn rejects_ambiguous_and_missing() {
        let dir = fresh_dir("ambiguous");
        std::fs::write(dir.join("f.txt"), "x\nx\n").unwrap();
        let tool = tool(&dir);
        let mut io = ToolIo::quiet();
        let output = tool.run(
            &serde_json::json!({"path": "f.txt", "old_string": "x", "new_string": "y"}),
            &mut io,
        );
        assert!(output.contains("appears 2 times"));
        let mut io = ToolIo::quiet();
        let output = tool.run(
            &serde_json::json!({"path": "f.txt", "old_string": "zz", "new_string": "y"}),
            &mut io,
        );
        assert!(output.contains("not found"));
    }
}
