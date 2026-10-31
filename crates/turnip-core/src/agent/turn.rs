//! Turn assembly: fold provider stream events into conversation items.
//!
//! A response is assembled from an open text buffer, an open reasoning
//! buffer, and tool calls accumulating argument fragments; sealing order is
//! reasoning first, then assistant text, then tool calls, matching how the
//! model produced them.

use crate::item::{Item, ItemOrigin, StreamUsage};
use crate::provider::StreamEvent;

/// A tool call whose arguments may still be streaming.
#[derive(Debug, Clone)]
pub struct PendingCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Default)]
pub struct TurnAssembly {
    /// Open assistant text buffer.
    pub text: String,
    /// Open reasoning buffer and the wire member it arrived in.
    reasoning_text: Option<String>,
    reasoning_field: Option<&'static str>,
    /// Sealed reasoning items (opaque signed blocks).
    sealed_reasoning: Vec<Item>,
    pub calls: Vec<PendingCall>,
    /// Items completed out of the open buffers, in arrival order.
    pub sealed: Vec<Item>,
    pub error_message: Option<String>,
    /// Usage recorded for the final attempt, or the failure's.
    pub usage: Option<StreamUsage>,
    /// Whether a Done event was seen.
    pub done: bool,
}

impl TurnAssembly {
    pub fn new() -> TurnAssembly {
        TurnAssembly::default()
    }

    /// Whether the assembly holds anything worth repairing an abort with:
    /// assistant text or a completed call. Sealed reasoning without text or
    /// calls evaporates with the turn.
    pub fn survives_abort(&self) -> bool {
        !self.text.is_empty()
            || !self.sealed.is_empty()
            || self
                .calls
                .iter()
                .any(|call| !call.arguments.is_empty())
    }

    /// Whether the assembly holds assistant text at all.
    pub fn has_text(&self) -> bool {
        !self.text.is_empty() || self.sealed.iter().any(|i| matches!(i, Item::Assistant { .. }))
    }

    /// Whether the model issued tool calls (sealed or pending).
    pub fn has_calls(&self) -> bool {
        !self.calls.is_empty()
            || self.sealed.iter().any(|i| matches!(i, Item::ToolCall { .. }))
    }

    /// Consume one stream event. `provider`/`model` bind reasoning provenance.
    pub fn consume(&mut self, event: &StreamEvent, provider: &str, model: &str) {
        match event {
            StreamEvent::TextDelta { text } => self.text.push_str(text),
            StreamEvent::ToolCallStart { id, name } => {
                self.flush_text();
                self.calls.push(PendingCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: String::new(),
                });
            }
            StreamEvent::ToolCallDelta { id, args_delta } => {
                // Route by id when present (Anthropic always sends it; some
                // Chat servers omit it on continuation deltas).
                let call = if !id.is_empty() {
                    self.calls.iter_mut().rev().find(|c| c.id == *id)
                } else {
                    self.calls.last_mut()
                };
                if let Some(call) = call {
                    call.arguments.push_str(args_delta);
                }
            }
            StreamEvent::ToolCallEnd { .. } => {}
            StreamEvent::ReasoningItem { json } => {
                self.sealed_reasoning.push(Item::Reasoning {
                    json: Some(json.clone()),
                    text: None,
                    field: None,
                    provider: Some(provider.to_string()),
                    model: Some(model.to_string()),
                });
            }
            StreamEvent::ReasoningDelta { text, field } => {
                self.reasoning_text
                    .get_or_insert_with(String::new)
                    .extend(text.iter().cloned());
                if self.reasoning_field.is_none() {
                    self.reasoning_field = *field;
                }
            }
            StreamEvent::Retry { .. } | StreamEvent::Progress { .. } => {}
            StreamEvent::Done { usage, .. } => {
                self.usage.get_or_insert_with(StreamUsage::empty).absorb(usage);
                self.done = true;
            }
            StreamEvent::Error { usage, message, .. } => {
                if self.error_message.is_none() {
                    self.error_message = Some(message.clone());
                }
                if let Some(usage) = usage {
                    self.usage.get_or_insert_with(StreamUsage::empty).absorb(usage);
                }
            }
        }
    }

    /// Record usage a failed attempt reported before dying (from a Retry
    /// event): a retried attempt is billed like a completed one.
    pub fn absorb_retry_usage(&mut self, usage: Option<&StreamUsage>) {
        if let Some(usage) = usage {
            self.usage.get_or_insert_with(StreamUsage::empty).absorb(usage);
        }
    }

    /// Seal the assembly into items after a completed stream.
    pub fn seal(&mut self) -> Vec<Item> {
        let mut items = std::mem::take(&mut self.sealed_reasoning);
        let reasoning = self.reasoning_text.take().filter(|t| !t.is_empty());
        if let Some(reasoning) = reasoning {
            items.push(Item::Reasoning {
                json: None,
                text: Some(reasoning),
                field: self.reasoning_field.take().map(String::from),
                provider: None,
                model: None,
            });
        }
        self.flush_text();
        items.append(&mut self.sealed);
        for call in std::mem::take(&mut self.calls) {
            items.push(Item::ToolCall {
                call_id: call.id,
                tool_name: call.name,
                arguments: call.arguments,
            });
        }
        items
    }
    fn flush_text(&mut self) {
        if !self.text.is_empty() {
            let text = std::mem::take(&mut self.text);
            self.sealed.push(Item::Assistant {
                text,
                origin: ItemOrigin::None,
            });
        }
    }

    /// Repair an aborted turn. Truncated reasoning re-sent as finished state
    /// derails models, so no repair replays it; an open (partial) tool call
    /// is dropped alongside, since a retry re-issues the same request.
    pub fn abort_repair(&mut self) {
        self.reasoning_text = None;
        self.reasoning_field = None;
        self.sealed_reasoning.clear();
        self.calls.clear();
        self.flush_text();
        for item in &mut self.sealed {
            if let Item::Assistant { origin, .. } = item {
                *origin = ItemOrigin::Interrupted;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_text_reasoning_and_calls() {
        let mut assembly = TurnAssembly::new();
        assembly.consume(
            &StreamEvent::ReasoningDelta {
                text: Some("hm".into()),
                field: Some("thinking"),
            },
            "p",
            "m",
        );
        assembly.consume(
            &StreamEvent::TextDelta { text: "Hello ".into() },
            "p",
            "m",
        );
        assembly.consume(
            &StreamEvent::TextDelta { text: "world".into() },
            "p",
            "m",
        );
        assembly.consume(
            &StreamEvent::ToolCallStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            "p",
            "m",
        );
        assembly.consume(
            &StreamEvent::ToolCallDelta {
                id: "c1".into(),
                args_delta: "{\"comma".into(),
            },
            "p",
            "m",
        );
        assembly.consume(
            &StreamEvent::ToolCallDelta {
                id: "c1".into(),
                args_delta: "nd\": \"ls\"}".into(),
            },
            "p",
            "m",
        );
        let items = assembly.seal();
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], Item::Reasoning { text: Some(t), field: Some(f), .. } if t == "hm" && f == "thinking"));
        assert!(matches!(&items[1], Item::Assistant { text, .. } if text == "Hello world"));
        assert!(matches!(&items[2], Item::ToolCall { call_id, tool_name, arguments }
            if call_id == "c1" && tool_name == "bash" && arguments == "{\"command\": \"ls\"}"));
    }

    #[test]
    fn abort_repair_keeps_text_drops_rest() {
        let mut assembly = TurnAssembly::new();
        assembly.consume(
            &StreamEvent::TextDelta { text: "partial".into() },
            "p",
            "m",
        );
        assembly.consume(
            &StreamEvent::ToolCallStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            "p",
            "m",
        );
        assembly.abort_repair();
        assert!(!assembly.survives_abort() || assembly.has_text());
        let items = assembly.seal();
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], Item::Assistant { origin: ItemOrigin::Interrupted, .. }));
    }
}
