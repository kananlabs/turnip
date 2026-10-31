//! The `read` tool: text files with 1-indexed offset/limit slicing, image
//! ingestion with dimensions, and binary rejection.

use serde_json::Value;

use super::image_sniff;
use super::{cap_output, display_path, resolve_path, Tool, ToolEnv, ToolIo};
use crate::item::{item_image_placeholder, ItemImage};
use crate::provider::ToolDef;
use crate::provider::ToolParam;
use crate::text;

const READ_DESCRIPTION: &str = "Read a text file's contents.\n\n\
    Use offset and limit for large files. Binary files and images are not returned as text; \
    images can be attached when the model supports them.";

pub struct ReadTool {
    pub env: ToolEnv,
}

impl Tool for ReadTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read",
            description: READ_DESCRIPTION,
            params: &[
                ToolParam {
                    name: "path",
                    r#type: "string",
                    item_type: None,
                    description: "Path to the file.",
                    required: true,
                },
                ToolParam {
                    name: "offset",
                    r#type: "integer",
                    item_type: None,
                    description: "1-indexed first line to return. Default 1.",
                    required: false,
                },
                ToolParam {
                    name: "limit",
                    r#type: "integer",
                    item_type: None,
                    description: "Maximum number of lines to return. Default: to EOF.",
                    required: false,
                },
            ],
        }
    }

    fn run(&self, args: &Value, io: &mut ToolIo) -> String {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return "error: 'path' is required".into();
        };
        let offset = args.get("offset").and_then(Value::as_i64).unwrap_or(1).max(1) as usize;
        let limit = args.get("limit").and_then(Value::as_i64).map(|l| l.max(1) as usize);

        let resolved = resolve_path(&self.env, path);
        let display = display_path(&self.env, &resolved);
        match std::fs::read(&resolved) {
            Err(error) => return format!("error: cannot read {display}: {error}"),
            Ok(bytes) => {
                if let Some(image) = image_sniff::sniff(&bytes) {
                    return self.ingest_image(io, &resolved, &display, &bytes, image);
                }
                if looks_binary(&bytes) {
                    return format!(
                        "error: {display} looks like a binary file ({} bytes)",
                        bytes.len()
                    );
                }
                self.read_text(io, &display, &bytes, offset, limit)
            }
        }
    }
}

impl ReadTool {
    fn ingest_image(
        &self,
        io: &mut ToolIo,
        resolved: &std::path::Path,
        display: &str,
        bytes: &[u8],
        image: image_sniff::SniffedImage,
    ) -> String {
        if !io.image_input {
            return format!(
                "error: {display} is an image ({}x{}), which this model cannot view",
                image.width, image.height
            );
        }
        if bytes.len() > 10 * 1024 * 1024 {
            return format!("error: {display} is too large to attach ({} bytes)", bytes.len());
        }
        let payload = ItemImage {
            mime: image.mime.to_string(),
            data_b64: {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(bytes)
            },
            width: image.width,
            height: image.height,
        };
        let placeholder = item_image_placeholder(&payload);
        io.images.push(payload);
        let _ = resolved;
        format!(
            "{placeholder}\nattached {} for viewing{}",
            display,
            if image.width > 0 {
                format!(" ({}x{})", image.width, image.height)
            } else {
                String::new()
            }
        )
    }

    fn read_text(
        &self,
        io: &mut ToolIo,
        display: &str,
        bytes: &[u8],
        offset: usize,
        limit: Option<usize>,
    ) -> String {
        let content = text::sanitize_utf8(bytes);
        let lines: Vec<&str> = content.split('\n').collect();
        // A trailing newline yields a final empty element; do not count it.
        let total = if lines.last() == Some(&"") {
            lines.len() - 1
        } else {
            lines.len()
        };
        let start = (offset - 1).min(total);
        let end = limit.map(|l| (start + l).min(total)).unwrap_or(total);
        let slice: Vec<&str> = lines[start..end].to_vec();
        let mut output = slice.join("\n");
        if offset > 1 || end < total {
            let note = format!("\n[lines {}-{} of {}]", start + 1, end, total);
            io.hidden_tail = note.len();
            output.push_str(&note);
        }
        if output.is_empty() {
            return format!("error: {display} is empty");
        }
        cap_output(&self.env, &output)
    }
}

/// Files with NUL bytes or a high control-byte ratio in the first 8 KiB are
/// treated as binary.
fn looks_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8192)];
    if head.contains(&0) {
        return true;
    }
    let controls = head
        .iter()
        .filter(|b| **b < 9 || (13 < **b && **b < 32))
        .count();
    !head.is_empty() && controls * 100 > head.len() * 10
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn binary_detection() {
        assert!(looks_binary(&[0u8, 1, 2, 3]));
        assert!(!looks_binary(b"plain text\nmore text\n"));
        let mut high_control = vec![b'a'; 9000];
        for byte in high_control.iter_mut().step_by(2) {
            *byte = 0x01;
        }
        assert!(looks_binary(&high_control));
    }

    #[test]
    fn reads_with_offset_and_limit() {
        let dir = std::env::temp_dir().join(format!("turnip-test-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "one\ntwo\nthree\nfour\n").unwrap();
        let tool = ReadTool {
            env: ToolEnv {
                cwd: dir.clone(),
                config: Config::default(),
            },
        };
        let mut io = ToolIo::quiet();
        let output = tool.run(
            &serde_json::json!({"path": "f.txt", "offset": 2, "limit": 2}),
            &mut io,
        );
        assert_eq!(output, "two\nthree\n[lines 2-3 of 4]");
        assert_eq!(io.hidden_tail, "\n[lines 2-3 of 4]".len());
        let mut io = ToolIo::quiet();
        let output = tool.run(&serde_json::json!({"path": "f.txt"}), &mut io);
        assert_eq!(output, "one\ntwo\nthree\nfour");
        let mut io = ToolIo::quiet();
        let output = tool.run(&serde_json::json!({"path": "missing.txt"}), &mut io);
        assert!(output.starts_with("error:"));
    }
}
