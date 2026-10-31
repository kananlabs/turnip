//! The generic HTTP provider: one implementation over the two wire dialects,
//! configured by a resolved endpoint.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use super::chat::{self, ChatDialect};
use super::messages::{self, MessagesDialect};
use super::{headers_from_json, Api, Context, Provider, StreamEvent, StreamObserver};
use crate::model::ModelInfo;
use crate::transport::{HttpTransport, RetryObserver, SseParser, StreamRequestError, Tick};
use crate::{CoreError, Result};

/// Which `/models` listing dialect an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataApi {
    /// Flat `{"data": [{"id": ...}]}` (also ollama/llama.cpp shapes).
    OpenAi,
    /// Paged `{"data": [...], "has_more": bool, "last_id": "..."}`.
    Anthropic,
}

/// A provider endpoint with every def/config overlay resolved.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    pub id: String,
    pub display_name: String,
    pub api: Api,
    pub base_url: String,
    pub api_key: Option<String>,
    pub local: bool,
    pub default_model: Option<String>,
    /// Accepted wire effort values, possibly empty.
    pub efforts: Vec<String>,
    pub chat: ChatDialect,
    pub messages: MessagesDialect,
    pub extra_headers: Option<String>,
    pub extra_body: Option<String>,
    pub metadata_api: MetadataApi,
}

/// A provider over an HTTP endpoint speaking Chat Completions or Messages.
pub struct HttpProvider {
    pub endpoint: ResolvedEndpoint,
    transport: HttpTransport,
    models: Mutex<Option<Vec<ModelInfo>>>,
}

impl HttpProvider {
    pub fn new(endpoint: ResolvedEndpoint, transport: HttpTransport) -> Self {
        HttpProvider {
            endpoint,
            transport,
            models: Mutex::new(None),
        }
    }

    /// Cached model list from the last successful [`Provider::list_models`].
    pub fn cached_models(&self) -> Option<Vec<ModelInfo>> {
        self.models.lock().ok().and_then(|guard| guard.clone())
    }

    fn auth_headers(&self, session_id: &str) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        match self.endpoint.api {
            Api::ChatCompletions => {
                if let Some(key) = &self.endpoint.api_key {
                    headers.insert("Authorization".into(), format!("Bearer {key}"));
                }
            }
            Api::Messages => {
                if let Some(key) = &self.endpoint.api_key {
                    headers.insert("x-api-key".into(), key.clone());
                }
                headers.insert("anthropic-version".into(), "2023-06-01".into());
            }
        }
        for (name, value) in headers_from_json(self.endpoint.extra_headers.as_deref(), session_id) {
            headers.insert(name, value);
        }
        headers
    }

    fn stream_url(&self) -> String {
        match self.endpoint.api {
            Api::ChatCompletions => format!("{}/chat/completions", self.endpoint.base_url),
            Api::Messages => format!("{}/messages", self.endpoint.base_url),
        }
    }

    fn emit_all(&self, events: Vec<StreamEvent>, observer: &mut dyn StreamObserver) -> Result<()> {
        for event in events {
            if !observer.on_event(event) {
                return Err(CoreError::Aborted);
            }
        }
        Ok(())
    }

    fn list_models_openai(&self, headers: &BTreeMap<String, String>) -> std::result::Result<Vec<ModelInfo>, String> {
        let url = format!("{}/models", self.endpoint.base_url);
        let body = self
            .transport
            .request("GET", &url, headers, None, Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        let array = value
            .get("data")
            .or_else(|| value.get("models"))
            .and_then(Value::as_array)
            .ok_or("response holds no model list")?;
        let mut models = Vec::new();
        for entry in array {
            let Some(id) = entry
                .get("id")
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let mut info = ModelInfo::new(id);
            info.description = entry
                .get("description")
                .or_else(|| entry.get("display_name"))
                .and_then(Value::as_str)
                .map(String::from);
            models.push(info);
        }
        Ok(models)
    }

    fn list_models_anthropic(&self, headers: &BTreeMap<String, String>) -> std::result::Result<Vec<ModelInfo>, String> {
        let mut models = Vec::new();
        let mut after: Option<String> = None;
        // Page until has_more clears; one round trip for typical setups.
        for _ in 0..16 {
            let mut url = format!("{}/models?limit=1000", self.endpoint.base_url);
            if let Some(after) = &after {
                url.push_str(&format!("&after={after}"));
            }
            let body = self
                .transport
                .request("GET", &url, headers, None, Some(Duration::from_secs(10)))
                .map_err(|e| e.to_string())?;
            let value: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            for entry in value.get("data").and_then(Value::as_array).into_iter().flatten() {
                let Some(id) = entry.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let mut info = ModelInfo::new(id);
                info.description = entry.get("display_name").and_then(Value::as_str).map(String::from);
                models.push(info);
            }
            match (
                value.get("has_more").and_then(Value::as_bool),
                value.get("last_id").and_then(Value::as_str),
            ) {
                (Some(true), Some(last)) => after = Some(last.to_string()),
                _ => break,
            }
        }
        Ok(models)
    }
}

impl Provider for HttpProvider {
    fn id(&self) -> &str {
        &self.endpoint.id
    }

    fn display_name(&self) -> &str {
        &self.endpoint.display_name
    }

    fn default_model(&self) -> Option<String> {
        self.endpoint.default_model.clone()
    }

    fn model_label(&self, model: &str) -> String {
        model.to_string()
    }

    fn default_effort(&self) -> Option<String> {
        // Most providers send no reasoning effort unless the user picks one;
        // local servers and backends without the concept stay unset.
        None
    }

    fn list_efforts(&self, _model: &str) -> Vec<String> {
        self.endpoint.efforts.clone()
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        self.cached_models()?
            .into_iter()
            .find(|info| info.id == model)
    }

    fn is_local(&self) -> bool {
        self.endpoint.local
    }

    fn list_models(&self) -> std::result::Result<Vec<ModelInfo>, String> {
        let headers = self.auth_headers("");
        let models = match self.endpoint.metadata_api {
            MetadataApi::OpenAi => self.list_models_openai(&headers)?,
            MetadataApi::Anthropic => self.list_models_anthropic(&headers)?,
        };
        if let Ok(mut guard) = self.models.lock() {
            *guard = Some(models.clone());
        }
        Ok(models)
    }

    fn stream(
        &self,
        context: &Context,
        model: &str,
        observer: &mut dyn StreamObserver,
    ) -> Result<()> {
        // Adapters keep a single mutable borrow of the observer while the
        // transport drives them.
        let effort_owned: Option<String> = match context.effort {
            None => None,
            Some(requested) => {
                if self.endpoint.efforts.is_empty() {
                    Some(requested.to_string())
                } else {
                    let set = crate::effort::EffortSet::from_iter(
                        self.endpoint.efforts.iter().map(String::as_str),
                    );
                    set.clamp(requested).map(String::from)
                }
            }
        };
        let clamped = Context {
            effort: effort_owned.as_deref(),
            ..*context
        };

        let mut body = match self.endpoint.api {
            Api::ChatCompletions => chat::build_body(&clamped, model, &self.endpoint.chat),
            Api::Messages => messages::build_body(&clamped, model, &self.endpoint.messages),
        };
        match self.endpoint.api {
            Api::ChatCompletions => {
                chat::merge_extra_body(&mut body, self.endpoint.extra_body.as_deref())
            }
            Api::Messages => chat::merge_extra_body(&mut body, self.endpoint.extra_body.as_deref()),
        }

        let mut headers = self.auth_headers(context.session_id);
        headers.insert("Accept".into(), "text/event-stream".into());
        let url = self.stream_url();

        let mut retry_observer = StreamRetry { observer };
        let reader = match self
            .transport
            .post_sse(&url, &headers, &body, &mut retry_observer)
        {
            Ok(reader) => reader,
            Err(StreamRequestError::Aborted) => return Err(CoreError::Aborted),
            Err(StreamRequestError::Http(error)) => {
                let _ = observer.on_event(StreamEvent::Error {
                    message: error.to_string(),
                    http_status: error.status,
                    usage: None,
                    response: None,
                });
                return Ok(());
            }
        };

        let idle_timeout_ms = self.transport.policy.idle_timeout_ms;
        match self.endpoint.api {
            Api::ChatCompletions => {
                let mut translator = chat::ChatTranslator::default();
                let mut parser = SseParser::new();
                loop {
                    let chunk = {
                        let mut tick = ObserverTick { observer };
                        reader.next_chunk(&mut tick, idle_timeout_ms)?
                    };
                    let Some(chunk) = chunk else { break };
                    for message in parser.feed(&chunk) {
                        let Ok(value) = serde_json::from_str::<Value>(&message.data) else {
                            continue;
                        };
                        self.emit_all(translator.feed_chunk(&value), observer)?;
                    }
                }
                self.emit_all(translator.finish(), observer)
            }
            Api::Messages => {
                let mut translator = messages::MessagesTranslator::default();
                let mut parser = SseParser::new();
                loop {
                    let chunk = {
                        let mut tick = ObserverTick { observer };
                        reader.next_chunk(&mut tick, idle_timeout_ms)?
                    };
                    let Some(chunk) = chunk else { break };
                    for message in parser.feed(&chunk) {
                        let Ok(value) = serde_json::from_str::<Value>(&message.data) else {
                            continue;
                        };
                        let (events, finished) = translator.feed(message.event.as_deref(), &value);
                        self.emit_all(events, observer)?;
                        if finished {
                            return Ok(());
                        }
                    }
                }
                self.emit_all(translator.finish(), observer)
            }
        }
    }
}

/// Feeds retry decisions and ticks to the transport during connection
/// establishment.
struct StreamRetry<'a> {
    observer: &'a mut dyn StreamObserver,
}

impl<'a> Tick for StreamRetry<'a> {
    fn tick(&mut self) -> bool {
        self.observer.tick()
    }
}

impl<'a> RetryObserver for StreamRetry<'a> {
    fn on_retry(&mut self, attempt: u32, max_attempts: u32, delay_ms: u64, status: Option<u16>) {
        let _ = self.observer.on_event(StreamEvent::Retry {
            attempt,
            max_attempts,
            delay_ms,
            http_status: status.unwrap_or(0),
        });
    }
}

/// Ticks the observer while the stream body is read.
struct ObserverTick<'a> {
    observer: &'a mut dyn StreamObserver,
}

impl<'a> Tick for ObserverTick<'a> {
    fn tick(&mut self) -> bool {
        self.observer.tick()
    }
}
