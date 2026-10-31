//! The `write` tool: full-content file writes with parent-directory creation
//! and a display-only diff against the previous contents.

use serde_json::Value;

use super::{display_path, resolve_path, Tool, ToolEnv, ToolIo};
use crate::provider::{ToolDef, ToolParam};
use crate::text;

const WRITE_DESCRIPTION: &str = "Write a file's full new contents, creating parent directories as needed.\n\n\
    Prefer `edit` for changes to an existing file; use `write` for new files and full rewrites.";

pub struct WriteTool {
    pub env: ToolEnv,
}

impl Tool for WriteTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "write",
            description: WRITE_DESCRIPTION,
            params: &[
                ToolParam {
                    name: "path",
                    r#type: "string",
                    item_type: None,
                    description: "Path to the file.",
                    required: true,
                },
                ToolParam {
                    name: "content",
                    r#type: "string",
                    item_type: None,
                    description: "Full new contents of the file.",
                    required: true,
                },
            ],
        }
    }

    fn run(&self, args: &Value, io: &mut ToolIo) -> String {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return "error: 'path' is required".into();
        };
        let Some(content) = args.get("content").and_then(Value::as_str) else {
            return "error: 'content' is required".into();
        };
        let resolved = resolve_path(&self.env, path);
        let display = display_path(&self.env, &resolved);
        let previous = std::fs::read_to_string(&resolved).ok();
        if let Some(parent) = resolved.parent() {
            if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
                return format!("error: cannot create directories for {display}");
            }
        }
        if let Err(error) = std::fs::write(&resolved, content) {
            return format!("error: cannot write {display}: {error}");
        }
        let lines = content.split('\n').count();
        let note = text::unified_diff(previous.as_deref().unwrap_or(""), content, 2);
        if !note.is_empty() {
            io.note(&note);
        }
        let mut output = format!("Wrote {display} ({lines} lines)");
        match previous {
            Some(_) => output.push_str(", replacing the previous contents"),
            None => output.push_str(", creating a new file"),
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn writes_and_reports() {
        let dir = std::env::temp_dir().join(format!("turnip-test-write-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = WriteTool {
            env: ToolEnv {
                cwd: dir.clone(),
                config: Config::default(),
            },
        };
        let mut io = ToolIo::quiet();
        let output = tool.run(
            &serde_json::json!({"path": "sub/f.txt", "content": "hello\n"}),
            &mut io,
        );
        assert_eq!(std::fs::read_to_string(dir.join("sub/f.txt")).unwrap(), "hello\n");
        assert_eq!(output, "Wrote sub/f.txt (2 lines), creating a new file");
    }
}
