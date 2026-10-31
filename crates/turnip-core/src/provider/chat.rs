//! OpenAI-compatible Chat Completions dialect: request construction and
//! stream-event translation. Shared by every `openai`-family provider.

use serde_json::{json, Map, Value};

use super::{Context, StreamEvent, StreamUsage, ToolDef};
use crate::item::{Item, ItemImage, StreamResponse};

/// Dialect settings resolved from the provider definition and configuration.
#[derive(Debug, Clone, Default)]
pub struct ChatDialect {
    /// Chat message member that carries prior reasoning back to the model
    /// (e.g. "reasoning_content" for DeepSeek/llama.cpp, "reasoning" for
    /// ollama). Omitted history reasoning derails thinking models.
    pub reasoning_roundtrip: Option<String>,
    /// Send `stream_options: {"include_usage": true}` so the final chunk
    /// reports token usage.
    pub send_stream_options: bool,
    /// Every assistant message carries the reasoning member, empty when there
    /// is nothing to replay.
    pub reasoning_required: bool,
}

fn image_part(image: &ItemImage) -> Value {
    json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{};base64,{}", image.mime, image.data_b64) },
    })
}

fn user_message(text: &str, images: &[ItemImage]) -> Value {
    if images.is_empty() {
        return json!({ "role": "user", "content": text });
    }
    let mut parts = vec![json!({ "type": "text", "text": text })];
    parts.extend(images.iter().map(image_part));
    json!({ "role": "user", "content": parts })
}

/// Build the request body for one Chat Completions stream.
pub fn build_body(context: &Context, model: &str, dialect: &ChatDialect) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = context.system_prompt {
        messages.push(json!({ "role": "system", "content": system }));
    }

    // Grouped assistant run: streamed text plus any tool calls it issued,
    // emitted as one assistant message carrying content and tool_calls.
    #[derive(Default)]
    struct Call {
        id: String,
        name: String,
        arguments: String,
    }
    let mut run: Option<(String, Option<String>, Vec<Call>)> = None; // (text, reasoning, calls)
    macro_rules! flush {
        () => {
            if let Some((text, reasoning, calls)) = run.take() {
                let has_calls = !calls.is_empty();
                if !text.is_empty() || has_calls {
                    let mut message = Map::new();
                    message.insert("role".into(), json!("assistant"));
                    if !text.is_empty() {
                        message.insert("content".into(), json!(text));
                    }
                    if let Some(member) = &dialect.reasoning_roundtrip {
                        if reasoning.is_some() || dialect.reasoning_required {
                            message.insert(
                                member.clone(),
                                json!(reasoning.unwrap_or_default()),
                            );
                        }
                    }
                    if has_calls {
                        let tool_calls: Vec<Value> = calls
                            .iter()
                            .map(|call| {
                                json!({
                                    "id": call.id,
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": call.arguments,
                                    },
                                })
                            })
                            .collect();
                        message.insert("tool_calls".into(), Value::Array(tool_calls));
                    }
                    messages.push(Value::Object(message));
                }
            }
        };
    }

    let mut pending_reasoning: Option<String> = None;
    for item in context.items {
        match item {
            Item::User { text, images, .. } => {
                flush!();
                messages.push(user_message(text, images));
            }
            Item::Assistant { text, .. } => {
                flush!();
                run = Some((text.clone(), pending_reasoning.take(), Vec::new()));
            }
            Item::ToolCall {
                call_id,
                tool_name,
                arguments,
            } => {
                let call = Call {
                    id: call_id.clone(),
                    name: tool_name.clone(),
                    arguments: arguments.clone(),
                };
                match &mut run {
                    Some((_, _, calls)) => calls.push(call),
                    None => {
                        run = Some((String::new(), pending_reasoning.take(), vec![call]))
                    }
                }
            }
            Item::ToolResult {
                call_id, output, ..
            } => {
                flush!();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output,
                }));
            }
            Item::Reasoning { text, .. } => {
                if pending_reasoning.is_none() {
                    pending_reasoning = text.clone();
                }
            }
            Item::Boundary | Item::Usage(_) => {}
        }
    }
    flush!();

    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(true));
    if dialect.send_stream_options {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if let Some(effort) = context.effort {
        if effort != "none" {
            body.insert("reasoning_effort".into(), json!(effort));
        }
    }
    if !context.tools.is_empty() {
        body.insert("tools".into(), json!(tool_definitions(context.tools)));
    }
    Value::Object(body)
}

fn tool_definitions(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.schema(),
                },
            })
        })
        .collect()
}

/// Merges the provider's extra body members (a JSON object) into the body.
pub fn merge_extra_body(body: &mut Value, extra: Option<&str>) {
    let Some(extra) = extra else { return };
    let Ok(Value::Object(extra)) = serde_json::from_str::<Value>(extra) else {
        return;
    };
    if let Value::Object(object) = body {
        for (key, value) in extra {
            object.insert(key, value);
        }
    }
}

/// Incremental translator from Chat Completions chunks to stream events.
#[derive(Debug, Default)]
pub struct ChatTranslator {
    /// Tool calls by wire index.
    calls: Vec<PendingCall>,
    usage: Option<StreamUsage>,
    response: Option<StreamResponse>,
    stop_reason: Option<String>,
    done_emitted: bool,
}

#[derive(Debug, Default)]
struct PendingCall {
    id: Option<String>,
    name: Option<String>,
    args: String,
    started: bool,
    delta_emitted: usize,
}

impl ChatTranslator {
    /// Feed one parsed chunk; returns the events it produced.
    pub fn feed_chunk(&mut self, chunk: &Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if let Some(error) = chunk.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider error")
                .to_string();
            events.push(StreamEvent::Error {
                message,
                http_status: None,
                usage: self.usage,
                response: self.response.clone(),
            });
            return events;
        }
        if let Some(usage) = parse_usage(chunk.get("usage")) {
            self.usage = Some(usage);
        }
        if let Some(model) = chunk.get("model").and_then(Value::as_str) {
            self.response
                .get_or_insert_with(StreamResponse::default)
                .model = Some(model.to_string());
        }
        for choice in chunk
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(reasoning) =
                choice.pointer("/delta/reasoning_content").and_then(Value::as_str)
            {
                events.push(StreamEvent::ReasoningDelta {
                    text: Some(reasoning.to_string()),
                    field: Some("reasoning_content"),
                });
            } else if let Some(reasoning) =
                choice.pointer("/delta/reasoning").and_then(Value::as_str)
            {
                events.push(StreamEvent::ReasoningDelta {
                    text: Some(reasoning.to_string()),
                    field: Some("reasoning"),
                });
            }
            if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str) {
                events.push(StreamEvent::TextDelta {
                    text: text.to_string(),
                });
            }
            if let Some(calls) = choice.pointer("/delta/tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.feed_tool_call(call, &mut events);
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.stop_reason = Some(reason.to_string());
            }
        }
        events
    }

    fn feed_tool_call(&mut self, call: &Value, events: &mut Vec<StreamEvent>) {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        while self.calls.len() <= index {
            self.calls.push(PendingCall::default());
        }
        let pending = &mut self.calls[index];
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            pending.id = Some(id.to_string());
        }
        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
            pending.name = Some(name.to_string());
        }
        if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
            pending.args.push_str(args);
        }
        if !pending.started {
            if let (Some(id), Some(name)) = (pending.id.clone(), pending.name.clone()) {
                pending.started = true;
                events.push(StreamEvent::ToolCallStart { id, name });
                // A provider may deliver all arguments with the start event.
                if !pending.args.is_empty() {
                    pending.delta_emitted = pending.args.len();
                    events.push(StreamEvent::ToolCallDelta {
                        id: pending.id.clone().expect("started"),
                        args_delta: pending.args.clone(),
                    });
                }
            }
        } else if let (Some(id), Some(args)) = (
            pending.id.clone(),
            call.pointer("/function/arguments").and_then(Value::as_str),
        ) {
            if pending.args.len() > pending.delta_emitted {
                // The whole accumulated tail since the last emission.
                pending.delta_emitted = pending.args.len();
                events.push(StreamEvent::ToolCallDelta {
                    id,
                    args_delta: args.to_string(),
                });
            }
        }
    }

    /// The stream ended; emit end-of-call events and the final Done.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if self.done_emitted {
            return events;
        }
        self.done_emitted = true;
        for call in &self.calls {
            if call.started {
                if let Some(id) = &call.id {
                    events.push(StreamEvent::ToolCallEnd { id: id.clone() });
                }
            }
        }
        events.push(StreamEvent::Done {
            stop_reason: self.stop_reason.clone(),
            usage: self.usage.unwrap_or_default(),
            response: self.response.clone(),
        });
        events
    }
}

fn parse_usage(value: Option<&Value>) -> Option<StreamUsage> {
    let usage = value?;
    if usage.as_object().is_none_or(|o| o.is_empty()) {
        return None;
    }
    let mut parsed = StreamUsage::default();
    parsed.input_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_i64)
        .or_else(|| usage.get("input_tokens").and_then(Value::as_i64));
    parsed.output_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_i64)
        .or_else(|| usage.get("output_tokens").and_then(Value::as_i64));
    parsed.cached_tokens = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_i64)
        .or_else(|| usage.get("cached_tokens").and_then(Value::as_i64));
    parsed.cost = usage.get("cost").and_then(Value::as_f64);
    Some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(items: &'a [Item], tools: &'a [ToolDef]) -> Context<'a> {
        Context {
            system_prompt: Some("sys"),
            items,
            tools,
            effort: None,
            image_input: false,
            session_id: "s",
        }
    }

    #[test]
    fn body_groups_assistant_runs_and_tools() {
        let items = vec![
            Item::user("do it"),
            Item::assistant("thinking about it"),
            Item::ToolCall {
                call_id: "call_1".into(),
                tool_name: "bash".into(),
                arguments: "{}".into(),
            },
            Item::tool_result("call_1", "ok"),
        ];
        let body = build_body(&context(&items, &[]), "m", &ChatDialect::default());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "do it");
        // Assistant text and tool call group into one message.
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2]["content"], "thinking about it");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[3]["role"], "tool");
    }

    #[test]
    fn body_includes_images_effort_tools_and_options() {
        let items = vec![Item::User {
            text: "look".into(),
            images: vec![ItemImage {
                mime: "image/png".into(),
                data_b64: "abc".into(),
                width: 2,
                height: 2,
            }],
            origin: Default::default(),
        }];
        let tool = ToolDef {
            name: "read",
            description: "Read a file.",
            params: &[],
        };
        let tools = [tool];
        let mut ctx = context(&items, &tools);
        ctx.effort = Some("high");
        let mut body = build_body(
            &ctx,
            "m",
            &ChatDialect {
                send_stream_options: true,
                ..Default::default()
            },
        );
        merge_extra_body(&mut body, Some(r#"{"temperature": 0}"#));
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["messages"][1]["content"][1]["type"], "image_url");
    }

    #[test]
    fn translator_handles_streaming_tool_call() {
        let mut translator = ChatTranslator::default();
        let start = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}}]});
        let events = translator.feed_chunk(&start);
        assert!(matches!(&events[0], StreamEvent::ToolCallStart { id, name } if id == "c1" && name == "read"));
        let delta = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"th\": 1}"}}]}}]});
        let events = translator.feed_chunk(&delta);
        assert!(matches!(&events[0], StreamEvent::ToolCallDelta { id, args_delta } if id == "c1" && args_delta == "th\": 1}"));
        let done = json!({"choices":[{"finish_reason":"tool_calls"}],
                          "usage":{"prompt_tokens":10,"completion_tokens":5}});
        translator.feed_chunk(&done);
        let events = translator.finish();
        assert!(matches!(&events[0], StreamEvent::ToolCallEnd { id } if id == "c1"));
        match &events[1] {
            StreamEvent::Done { usage, stop_reason, .. } => {
                assert_eq!(usage.input_tokens, Some(10));
                assert_eq!(usage.output_tokens, Some(5));
                assert_eq!(stop_reason.as_deref(), Some("tool_calls"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn translator_emits_reasoning_and_error() {
        let mut translator = ChatTranslator::default();
        let events = translator.feed_chunk(&json!({
            "choices":[{"delta":{"reasoning_content":"deep thought","content":"answer"}}]
        }));
        assert!(matches!(&events[0], StreamEvent::ReasoningDelta { field: Some("reasoning_content"), .. }));
        assert!(matches!(&events[1], StreamEvent::TextDelta { text } if text == "answer"));
        let events = translator.feed_chunk(&json!({"error": {"message": "boom"}}));
        assert!(matches!(&events[0], StreamEvent::Error { message, .. } if message == "boom"));
    }
}
