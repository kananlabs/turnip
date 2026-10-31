//! The provider abstraction: one trait, provider-independent stream events,
//! and the request context. Adapters translate [`Item`]s into their wire
//! format and wire responses back into events.

pub mod chat;
pub mod http;
pub mod messages;
pub mod registry;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::item::{Item, StreamResponse, StreamUsage};
use crate::model::ModelInfo;
use crate::{CoreError, Result};

/// Cooperative cancellation, shared with tool execution and streams.
#[derive(Clone, Default)]
pub struct AbortSignal {
    aborted: Arc<AtomicBool>,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }
}

/// Which wire dialect an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Api {
    /// OpenAI-compatible Chat Completions.
    #[default]
    ChatCompletions,
    /// Anthropic-compatible Messages.
    Messages,
}

/// One JSON Schema property advertised for a tool.
#[derive(Debug, Clone)]
pub struct ToolParam {
    pub name: &'static str,
    /// JSON Schema primitive: "string", "integer", "boolean", or "array".
    pub r#type: &'static str,
    /// Element type when `r#type` is "array".
    pub item_type: Option<&'static str>,
    pub description: &'static str,
    pub required: bool,
}

/// A tool advertisement sent to the model.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub params: &'static [ToolParam],
}

impl ToolDef {
    /// The JSON Schema object describing this tool's parameters.
    pub fn schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for param in self.params {
            let mut schema = serde_json::Map::new();
            schema.insert("type".into(), Value::String(param.r#type.into()));
            if let Some(item_type) = param.item_type {
                schema.insert(
                    "items".into(),
                    serde_json::json!({ "type": item_type }),
                );
            }
            schema.insert(
                "description".into(),
                Value::String(param.description.into()),
            );
            properties.insert(param.name.into(), Value::Object(schema));
            if param.required {
                required.push(Value::String(param.name.into()));
            }
        }
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), Value::String("object".into()));
        schema.insert("properties".into(), Value::Object(properties));
        schema.insert("required".into(), Value::Array(required));
        schema.insert("additionalProperties".into(), Value::Bool(false));
        Value::Object(schema)
    }
}

/// The provider-independent request context handed to [`Provider::stream`].
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub system_prompt: Option<&'a str>,
    /// The model-visible conversation, starting at the newest compaction seed.
    pub items: &'a [Item],
    pub tools: &'a [ToolDef],
    /// Wire reasoning-effort value, or None to let the provider choose.
    pub effort: Option<&'a str>,
    /// Resolved image-input capability for the active model.
    pub image_input: bool,
    /// Stable conversation id for provider-side affinity (sticky routing and
    /// prompt-cache keys).
    pub session_id: &'a str,
}

impl<'a> Context<'a> {
    pub fn empty() -> Context<'a> {
        Context {
            system_prompt: None,
            items: &[],
            tools: &[],
            effort: None,
            image_input: false,
            session_id: "",
        }
    }
}

/// Events emitted by a provider's stream. Payloads are owned; events are
/// consumed one at a time by the callback, which returns false to abort.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta {
        text: String,
    },
    /// Tool call id and name known.
    ToolCallStart {
        id: String,
        name: String,
    },
    /// Partial JSON arguments for a started call.
    ToolCallDelta {
        id: String,
        args_delta: String,
    },
    /// Arguments finalized for a call.
    ToolCallEnd {
        id: String,
    },
    /// Opaque provider state to preserve for the next request.
    ReasoningItem {
        json: String,
    },
    /// Text may be empty to signal reasoning activity without exposing
    /// content. `field` names the wire member it arrived in.
    ReasoningDelta {
        text: Option<String>,
        field: Option<&'static str>,
    },
    /// A transient attempt failed; another will follow.
    Retry {
        /// 1-based attempt that just failed.
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        /// 0 = transport error, otherwise the HTTP code.
        http_status: u16,
    },
    /// Prompt-prefill progress (local servers).
    Progress {
        processed: i64,
        total: i64,
        cache: i64,
    },
    Done {
        stop_reason: Option<String>,
        usage: StreamUsage,
        response: Option<StreamResponse>,
    },
    Error {
        message: String,
        http_status: Option<u16>,
        /// Usage reported before the failure; truncated responses are billed
        /// like complete ones.
        usage: Option<StreamUsage>,
        response: Option<StreamResponse>,
    },
}

/// Observes a provider stream. `on_event` and `tick` let the frontend render
/// and cancel; returning false from either aborts the stream.
pub trait StreamObserver {
    fn on_event(&mut self, event: StreamEvent) -> bool;
    /// Pumped periodically while the stream runs (waiting on the socket,
    /// between retries). Also drives animations.
    fn tick(&mut self) -> bool;
}

/// A no-op observer for tests and one-shot consumers.
pub struct QuietObserver;

impl StreamObserver for QuietObserver {
    fn on_event(&mut self, _event: StreamEvent) -> bool {
        true
    }
    fn tick(&mut self) -> bool {
        true
    }
}

/// A provider connection: one configured endpoint able to run conversations.
pub trait Provider: Send {
    /// Stable selectable id (never a display label).
    fn id(&self) -> &str;
    /// Banner and picker label.
    fn display_name(&self) -> &str;
    /// A safe default model derived from live state, when one exists.
    fn default_model(&self) -> Option<String>;
    /// Display label for a model; the wire id itself when unremarkable.
    fn model_label(&self, model: &str) -> String;
    /// Default wire effort for new conversations, if any.
    fn default_effort(&self) -> Option<String>;
    /// Accepted wire effort values for `model`, in provider order (possibly empty).
    fn list_efforts(&self, model: &str) -> Vec<String>;
    /// Metadata for `model` when the provider knows it (context window, limits).
    fn model_info(&self, model: &str) -> Option<ModelInfo>;
    /// Local servers need no key and are probed for availability.
    fn is_local(&self) -> bool;
    /// Enumerate models. Errors are user-actionable strings.
    fn list_models(&self) -> std::result::Result<Vec<ModelInfo>, String>;
    /// Stream one response for `model` over `context`. API-level failures are
    /// reported through [`StreamEvent::Error`]; the return value covers
    /// transport-level fatals and aborts.
    fn stream(
        &self,
        context: &Context,
        model: &str,
        observer: &mut dyn StreamObserver,
    ) -> Result<()>;
}

/// Extract a user-presentable error message from an HTTP error body.
pub fn api_error_message(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.chars().take(500).collect();
    };
    // Common shapes: {"error": {"message": ...}}, {"error": "..."},
    // {"message": "...}, {"detail": "..."}.
    if let Some(message) = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("detail"))
        .and_then(Value::as_str)
    {
        return message.to_string();
    }
    if let Some(message) = value.get("error").and_then(Value::as_str) {
        return message.to_string();
    }
    body.chars().take(500).collect()
}

/// Serialize headers from a `{"name": "value"}` JSON object, substituting
/// `{session_id}` in values.
pub fn headers_from_json(
    object: Option<&str>,
    session_id: &str,
) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    let Some(object) = object else {
        return headers;
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(object) else {
        return headers;
    };
    for (name, value) in map {
        if let Some(value) = value.as_str() {
            headers.insert(
                name.clone(),
                value.replace("{session_id}", session_id).to_string(),
            );
        }
    }
    headers
}

/// Convenience for fatal transport errors surfaced to callers.
pub fn transport_abort() -> CoreError {
    CoreError::Aborted
}

/// Record describing a provider definition, resolved from the registry and
/// configuration. Used by pickers and autoselection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: String,
    pub display_name: String,
    pub api: Api,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub local: bool,
    /// Why the provider cannot be used when unconfigured.
    pub unconfigured_reason: Option<String>,
    pub internal: bool,
}

impl Default for ProviderDescriptor {
    fn default() -> Self {
        ProviderDescriptor {
            id: String::new(),
            display_name: String::new(),
            api: Api::ChatCompletions,
            base_url: None,
            api_key_env: None,
            local: false,
            unconfigured_reason: None,
            internal: false,
        }
    }
}
