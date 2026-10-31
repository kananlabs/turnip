//! Tool definitions and execution. Tools run synchronously on the agent
//! thread and report progress through the callbacks in [`ToolIo`]; their
//! return value is the model-facing output (recoverable errors included).

pub mod bash;
pub mod edit;
pub mod image_sniff;
pub mod read;
pub mod write;

use std::path::PathBuf;

use serde_json::Value;

use super::provider::ToolDef;
use crate::config::Config;
use crate::item::ItemImage;

/// Everything a tool needs to run: the working directory to resolve paths
/// against, and the configuration (output caps, timeouts, shell).
#[derive(Debug, Clone)]
pub struct ToolEnv {
    pub cwd: PathBuf,
    pub config: Config,
}

/// Receives live tool output while it runs. Implemented by the frontend
/// (and by [`NullSink`] for tests and library consumers).
pub trait ToolSink {
    /// Live output bytes, for terminal display as they arrive.
    fn display(&mut self, bytes: &[u8]);
    /// Display-only note (e.g. a diff preview); never sent to the model.
    fn note(&mut self, text: &str);
    /// Pumped while the tool waits; returning false interrupts it.
    fn tick(&mut self) -> bool;
}

/// A sink that discards output and never interrupts.
pub struct NullSink;

impl ToolSink for NullSink {
    fn display(&mut self, _bytes: &[u8]) {}
    fn note(&mut self, _text: &str) {}
    fn tick(&mut self) -> bool {
        true
    }
}

/// Per-call channels between a running tool and the frontend.
pub struct ToolIo<'a> {
    pub sink: Option<&'a mut dyn ToolSink>,
    /// Resolved image-input capability for the active model.
    pub image_input: bool,
    /// Images the tool attached for the next request (base64 + metadata).
    pub images: Vec<ItemImage>,
    /// Length of a trailing model-only annotation in the returned output.
    pub hidden_tail: usize,
    /// The run was cut short by a user interrupt.
    pub interrupted: bool,
}

impl<'a> ToolIo<'a> {
    /// A disconnected io context: no display, no interruption.
    pub fn quiet() -> ToolIo<'a> {
        ToolIo {
            sink: None,
            image_input: false,
            images: Vec::new(),
            hidden_tail: 0,
            interrupted: false,
        }
    }

    fn display(&mut self, bytes: &[u8]) {
        if let Some(sink) = self.sink.as_mut() {
            sink.display(bytes);
        }
    }

    fn note(&mut self, text: &str) {
        if let Some(sink) = self.sink.as_mut() {
            sink.note(text);
        }
    }

    /// Pump the frontend once; false interrupts the tool.
    pub fn tick(&mut self) -> bool {
        match self.sink.as_mut() {
            Some(sink) => sink.tick(),
            None => true,
        }
    }
}

/// One executable agent tool.
pub trait Tool: Send + Sync {
    fn def(&self) -> ToolDef;
    /// Execute with parsed arguments; the return value is the model-facing
    /// output, including recoverable errors.
    fn run(&self, args: &Value, io: &mut ToolIo) -> String;
}

/// The tool set advertised to the model and dispatched by the agent loop.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(env: ToolEnv) -> ToolRegistry {
        ToolRegistry {
            tools: vec![
                Box::new(read::ReadTool { env: env.clone() }),
                Box::new(edit::EditTool { env: env.clone() }),
                Box::new(write::WriteTool { env: env.clone() }),
                Box::new(bash::BashTool { env }),
            ],
        }
    }

    /// Definitions advertised to the model.
    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools.iter().map(|tool| tool.def()).collect()
    }

    pub fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .map(|tool| tool.as_ref())
            .find(|tool| tool.def().name == name)
    }

    /// Dispatch one call, answering unknown tools and malformed arguments
    /// with recoverable errors the model can act on.
    pub fn dispatch(&self, name: &str, arguments: &str, io: &mut ToolIo) -> String {
        let Some(tool) = self.find(name) else {
            return format!("error: unknown tool '{name}'");
        };
        let args: Value = match serde_json::from_str(arguments) {
            Ok(Value::Object(map)) => Value::Object(map),
            Ok(_) => return "error: tool arguments must be a JSON object".to_string(),
            Err(error) => return format!("error: invalid arguments JSON: {error}"),
        };
        tool.run(&args, io)
    }
}

/// Resolve a tool path argument against the tool environment's cwd.
pub(crate) fn resolve_path(env: &ToolEnv, path: &str) -> PathBuf {
    let expanded = shellexpand_home(env, path);
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        env.cwd.join(path)
    }
}

/// `~` expands to the home directory in tool path arguments.
pub(crate) fn shellexpand_home(env: &ToolEnv, path: &str) -> String {
    if path == "~" {
        return dirs::home_dir()
            .unwrap_or_else(|| env.cwd.clone())
            .to_string_lossy()
            .into_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// Display path relative to the tool cwd when inside it (compact tool lines).
pub(crate) fn display_path(env: &ToolEnv, path: &PathBuf) -> String {
    if let Ok(relative) = path.strip_prefix(&env.cwd) {
        let relative = relative.to_string_lossy().into_owned();
        if !relative.is_empty() {
            return relative;
        }
    }
    path.to_string_lossy().into_owned()
}

/// Head+tail truncation of a tool output to the configured cap, with a
/// model-facing elision note appended.
pub(crate) fn cap_output(env: &ToolEnv, output: &str) -> String {
    let cap = env.config.size_or("tool_output_cap", 50 * 1024) as usize;
    let bytes = output.as_bytes();
    if bytes.len() <= cap {
        return output.to_string();
    }
    let mut cut = crate::text::truncate_head_tail(bytes, cap);
    // Keep the string valid UTF-8.
    while let Err(_) = std::str::from_utf8(&cut) {
        cut.pop();
    }
    String::from_utf8(cut).expect("valid utf-8 after trimming")
}
