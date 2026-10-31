//! Anthropic-compatible Messages dialect: request construction and
//! stream-event translation.

use serde_json::{json, Map, Value};

use super::{Context, StreamEvent, StreamUsage};
use crate::item::{Item, ItemImage, StreamResponse};

/// Dialect settings resolved from the provider definition and configuration.
#[derive(Debug, Clone)]
pub struct MessagesDialect {
    /// Default `max_tokens` when model metadata reports none.
    pub max_tokens: i64,
    /// The endpoint validates thinking-block signatures like the first-party
    /// API: unsigned thinking blocks (from other backends) are dropped rather
    /// than replayed and rejected.
    pub strict_signatures: bool,
}

impl Default for MessagesDialect {
    fn default() -> Self {
        MessagesDialect {
            max_tokens: 16_384,
            strict_signatures: true,
        }
    }
}

/// Thinking budget in tokens for a categorical effort level (minimum the API
/// accepts is 1024).
pub fn effort_budget(effort: &str) -> Option<i64> {
    match effort {
        "minimal" | "low" => Some(2048),
        "medium" => Some(4096),
        "high" => Some(8192),
        "xhigh" => Some(12_288),
        "max" => Some(16_384),
        _ => None,
    }
}

fn image_block(image: &ItemImage) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": image.mime,
            "data": image.data_b64,
        },
    })
}

fn parse_tool_arguments(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or(json!({}))
}

/// Build the request body for one Messages stream.
pub fn build_body(context: &Context, model: &str, dialect: &MessagesDialect) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("stream".into(), json!(true));
    if let Some(system) = context.system_prompt {
        body.insert(
            "system".into(),
            json!([{ "type": "text", "text": system }]),
        );
    }

    // Message runs: same-role items group into one message. Assistant runs may
    // hold thinking, text, and tool_use parts; user runs hold tool results,
    // text, and images.
    let mut runs: Vec<(bool, Vec<Value>)> = Vec::new(); // (is_assistant, parts)
    let push_part = |runs: &mut Vec<(bool, Vec<Value>)>, assistant: bool, part: Value| {
        match runs.last_mut() {
            Some((is_assistant, parts)) if *is_assistant == assistant => parts.push(part),
            _ => runs.push((assistant, vec![part])),
        }
    };

    for item in context.items {
        match item {
            Item::User { text, images, .. } => {
                if !text.is_empty() {
                    push_part(&mut runs, false, json!({ "type": "text", "text": text }));
                }
                for image in images {
                    push_part(&mut runs, false, image_block(image));
                }
            }
            Item::Assistant { text, .. } => {
                if !text.is_empty() {
                    push_part(&mut runs, true, json!({ "type": "text", "text": text }));
                }
            }
            Item::ToolCall {
                call_id,
                tool_name,
                arguments,
            } => {
                push_part(
                    &mut runs,
                    true,
                    json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": tool_name,
                        "input": parse_tool_arguments(arguments),
                    }),
                );
            }
            Item::ToolResult {
                call_id,
                output,
                images,
                ..
            } => {
                let mut content = vec![json!({ "type": "text", "text": output })];
                for image in images {
                    content.push(image_block(image));
                }
                push_part(
                    &mut runs,
                    false,
                    json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": content,
                    }),
                );
            }
            Item::Reasoning { json, text, .. } => {
                // Prefer the opaque signed block; fall back to plain text,
                // which strict endpoints reject and therefore lose.
                let (thinking, signature) = if let Some(block) =
                    json.as_ref().and_then(|j| serde_json::from_str::<Value>(j).ok())
                {
                    (
                        thinking_member(&block),
                        block.get("signature").and_then(Value::as_str).map(String::from),
                    )
                } else {
                    (text.clone(), None)
                };
                let Some(thinking) = thinking.filter(|t| !t.is_empty()) else {
                    continue;
                };
                if signature.is_none() && dialect.strict_signatures {
                    continue; // would be rejected on replay anyway
                }
                let mut block = Map::new();
                block.insert("type".into(), json!("thinking"));
                block.insert("thinking".into(), json!(thinking));
                if let Some(signature) = signature {
                    block.insert("signature".into(), json!(signature));
                }
                push_part(&mut runs, true, Value::Object(block));
            }
            Item::Boundary | Item::Usage(_) => {}
        }
    }

    let messages: Vec<Value> = runs
        .into_iter()
        .map(|(assistant, parts)| {
            json!({ "role": if assistant { "assistant" } else { "user" }, "content": parts })
        })
        .collect();
    body.insert("messages".into(), Value::Array(messages));

    let mut max_tokens = dialect.max_tokens;
    if let Some(effort) = context.effort {
        if let Some(budget) = effort_budget(effort) {
            body.insert(
                "thinking".into(),
                json!({ "type": "enabled", "budget_tokens": budget }),
            );
            // max_tokens must exceed the thinking budget.
            max_tokens = max_tokens.max(budget + 4096);
        }
    }
    body.insert("max_tokens".into(), json!(max_tokens));

    if !context.tools.is_empty() {
        let tools: Vec<Value> = context
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.schema(),
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    Value::Object(body)
}

fn thinking_member(block: &Value) -> Option<String> {
    block
        .get("thinking")
        .or_else(|| block.get("text"))
        .and_then(Value::as_str)
        .map(String::from)
}

/// Incremental translator from Messages SSE events to stream events.
#[derive(Debug, Default)]
pub struct MessagesTranslator {
    usage: StreamUsage,
    response: Option<StreamResponse>,
    stop_reason: Option<String>,
    /// (thinking, signature) for the currently open thinking block.
    thinking: Option<(String, String)>,
    /// Index of the currently open content block, with its open tool-use id.
    open_block: Option<(u64, Option<String>)>,
    done_emitted: bool,
}

impl MessagesTranslator {
    /// Feed one SSE message (`event` + parsed `data`); returns its events.
    /// Returns `true` when the stream is finished (message_stop or error).
    pub fn feed(&mut self, event: Option<&str>, data: &Value) -> (Vec<StreamEvent>, bool) {
        let mut events = Vec::new();
        let name = event.unwrap_or_else(|| {
            data.get("type").and_then(Value::as_str).unwrap_or_default()
        });
        match name {
            "message_start" => {
                let message = data.pointer("/message").cloned().unwrap_or_default();
                self.usage.input_tokens = message
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_i64);
                self.usage.cache_write_tokens = message
                    .pointer("/usage/cache_write_input_tokens")
                    .and_then(Value::as_i64);
                self.usage.cache_write_1h_tokens = message
                    .pointer("/usage/cache_creation_input_tokens")
                    .and_then(Value::as_i64)
                    .or_else(|| {
                        message
                            .pointer("/usage/cache_creation")
                            .and_then(|c| c.get("ephemeral_1h_input_tokens"))
                            .and_then(Value::as_i64)
                    });
                self.usage.cached_tokens = message
                    .pointer("/usage/cache_read_input_tokens")
                    .and_then(Value::as_i64);
                if let Some(model) = message.get("model").and_then(Value::as_str) {
                    self.response
                        .get_or_insert_with(StreamResponse::default)
                        .model = Some(model.to_string());
                }
            }
            "content_block_start" => {
                let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = data
                    .pointer("/content_block")
                    .cloned()
                    .unwrap_or_default();
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        self.open_block = Some((index, Some(id.clone())));
                        events.push(StreamEvent::ToolCallStart {
                            id,
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .into(),
                        });
                    }
                    Some("thinking") => {
                        self.open_block = Some((index, None));
                        self.thinking = Some((String::new(), String::new()));
                    }
                    _ => self.open_block = Some((index, None)),
                }
            }
            "content_block_delta" => {
                let delta = data.pointer("/delta").cloned().unwrap_or_default();
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => events.push(StreamEvent::TextDelta {
                        text: delta.get("text").and_then(Value::as_str).unwrap_or_default().into(),
                    }),
                    Some("input_json_delta") => {
                        if let Some((_, Some(id))) = &self.open_block {
                            events.push(StreamEvent::ToolCallDelta {
                                id: id.clone(),
                                args_delta: delta
                                    .get("partial_json")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .into(),
                            });
                        }
                    }
                    Some("thinking_delta") => {
                        let text = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some((thinking, _)) = &mut self.thinking {
                            thinking.push_str(text);
                        }
                        events.push(StreamEvent::ReasoningDelta {
                            text: Some(text.into()),
                            field: Some("thinking"),
                        });
                    }
                    Some("signature_delta") => {
                        if let Some((_, signature)) = &mut self.thinking {
                            signature.push_str(
                                delta.get("signature").and_then(Value::as_str).unwrap_or_default(),
                            );
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some((_, tool_id)) = self.open_block.take() {
                    if let Some(id) = tool_id {
                        events.push(StreamEvent::ToolCallEnd { id });
                    } else if let Some((thinking, signature)) = self.thinking.take() {
                        if !thinking.is_empty() {
                            events.push(StreamEvent::ReasoningItem {
                                json: json!({ "thinking": thinking, "signature": signature })
                                    .to_string(),
                            });
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(output) = data.pointer("/usage/output_tokens").and_then(Value::as_i64) {
                    self.usage.output_tokens = Some(output);
                }
            }
            "message_stop" => {
                events.push(StreamEvent::Done {
                    stop_reason: self.stop_reason.clone(),
                    usage: self.usage,
                    response: self.response.clone(),
                });
                self.done_emitted = true;
                return (events, true);
            }
            "error" => {
                let message = data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error");
                events.push(StreamEvent::Error {
                    message: message.into(),
                    http_status: None,
                    usage: Some(self.usage),
                    response: self.response.clone(),
                });
                return (events, true);
            }
            "ping" | "completion" => {}
            _ => {}
        }
        (events, false)
    }

    /// The stream ended without message_stop; synthesize a Done so the turn
    /// still records what it received.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        if self.done_emitted {
            return Vec::new();
        }
        self.done_emitted = true;
        vec![StreamEvent::Done {
            stop_reason: self.stop_reason.clone(),
            usage: self.usage,
            response: self.response.clone(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_builds_runs_and_tool_results() {
        let items = vec![
            Item::user("go"),
            Item::assistant("hmm"),
            Item::ToolCall {
                call_id: "t1".into(),
                tool_name: "bash".into(),
                arguments: "{\"command\": \"ls\"}".into(),
            },
            Item::tool_result("t1", "files"),
            Item::user("thanks"),
        ];
        let context = Context {
            system_prompt: Some("sys"),
            items: &items,
            tools: &[],
            effort: None,
            image_input: false,
            session_id: "s",
        };
        let body = build_body(&context, "claude", &MessagesDialect::default());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "text");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[1]["content"][1]["input"]["command"], "ls");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][1]["type"], "text");
        assert_eq!(body["max_tokens"], 16384);
    }

    #[test]
    fn thinking_budget_scales_max_tokens() {
        let items = vec![Item::user("go")];
        let context = Context {
            system_prompt: None,
            items: &items,
            tools: &[],
            effort: Some("high"),
            image_input: false,
            session_id: "s",
        };
        let body = build_body(&context, "claude", &MessagesDialect::default());
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
        assert_eq!(body["max_tokens"], 16384);
    }

    #[test]
    fn unsigned_thinking_dropped_when_strict() {
        let items = vec![
            Item::user("go"),
            Item::Reasoning {
                json: None,
                text: Some("internal thought".into()),
                field: Some("thinking".to_string()),
                provider: None,
                model: None,
            },
            Item::assistant("answer"),
        ];
        let context = Context {
            system_prompt: None,
            items: &items,
            tools: &[],
            effort: None,
            image_input: false,
            session_id: "s",
        };
        let strict = build_body(&context, "m", &MessagesDialect::default());
        // The assistant run keeps only the text part: unsigned thinking dropped.
        assert_eq!(strict["messages"][1]["content"].as_array().unwrap().len(), 1);
        let lax = build_body(
            &context,
            "m",
            &MessagesDialect {
                max_tokens: 1024,
                strict_signatures: false,
            },
        );
        assert_eq!(lax["messages"][1]["content"][0]["type"], "thinking");
    }

    #[test]
    fn translator_streams_text_then_done() {
        let mut translator = MessagesTranslator::default();
        let (events, done) = translator.feed(
            Some("message_start"),
            &json!({"message": {"usage": {"input_tokens": 7}, "model": "claude-x"}}),
        );
        assert!(!done);
        assert!(events.is_empty());
        let (events, done) = translator.feed(
            Some("content_block_delta"),
            &json!({"delta": {"type": "text_delta", "text": "he"}}),
        );
        assert!(!done);
        assert!(matches!(&events[0], StreamEvent::TextDelta { text } if text == "he"));
        let (_events, done) = translator.feed(
            Some("message_delta"),
            &json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        );
        assert!(!done);
        let (events, done) = translator.feed(Some("message_stop"), &json!({}));
        assert!(done);
        match &events[0] {
            StreamEvent::Done { usage, stop_reason, response } => {
                assert_eq!(usage.input_tokens, Some(7));
                assert_eq!(usage.output_tokens, Some(2));
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(response.as_ref().unwrap().model.as_deref(), Some("claude-x"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn translator_seals_signed_thinking() {
        let mut translator = MessagesTranslator::default();
        translator.feed(
            Some("content_block_start"),
            &json!({"content_block": {"type": "thinking"}}),
        );
        let _ = translator.feed(
            Some("content_block_delta"),
            &json!({"delta": {"type": "thinking_delta", "thinking": "ponder"}}),
        );
        let _ = translator.feed(
            Some("content_block_delta"),
            &json!({"delta": {"type": "signature_delta", "signature": "sig1"}}),
        );
        let (events, _) = translator.feed(Some("content_block_stop"), &json!({}));
        match &events[0] {
            StreamEvent::ReasoningItem { json } => {
                let value: Value = serde_json::from_str(json).unwrap();
                assert_eq!(value["thinking"], "ponder");
                assert_eq!(value["signature"], "sig1");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
