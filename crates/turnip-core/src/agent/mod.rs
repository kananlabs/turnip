//! The agent core: the canonical conversation state shared by every
//! frontend (interactive REPL, one-shot, and future bindings), plus the
//! turn assembly, tool loop, compaction, and environment assembly.

pub mod compact;
pub mod env;
pub mod r#loop;
pub mod turn;

pub use r#loop::{run_user_turn, PendingCallView, TurnHooks, TurnOutcome};

use crate::config::Config;
use crate::item::{
    items_context_floor, Item, ItemOrigin, StreamUsage, TurnProvenance, TurnUsage,
};
use crate::provider::{Context, Provider, ToolDef};

/// Synthetic history content keeping interrupted turns well formed. Origin,
/// not text, identifies continuations.
pub const CONTINUE_MARKER: &str = "[continue]";
pub const INTERRUPT_MARKER: &str = "[interrupted]";
pub const REFUSED_RESULT: &str = "error: tool calls are disabled in this session";

/// Frontend-independent agent state: the flat item log with its views and
/// mutators, plus the effective request settings.
pub struct AgentSession {
    /// Stable provider id stamped into provenance (never a display label).
    pub provider_id: String,
    pub provider_label: String,
    /// Exact model id; None when unresolved (the frontend prompts for one).
    pub model: Option<String>,
    pub model_label: Option<String>,
    /// Wire reasoning effort; None omits it.
    pub effort: Option<String>,
    pub system_prompt: String,
    /// Send only user content and advertise no tools.
    pub raw_mode: bool,
    pub tools: Vec<ToolDef>,
    /// The whole conversation, including prefixes a compaction summarized.
    pub items: Vec<Item>,
    /// Items /undo removed, in original order: no longer context, but the
    /// requests they recorded were still served and paid for.
    pub retired: Vec<Item>,
    /// Wall time of completed user turns, in milliseconds.
    pub worked_ms: i64,
    pub last_user_turn_ms: i64,
    /// Stable id for provider-side affinity (sticky routing, prompt caches).
    pub session_id: String,
}

impl AgentSession {
    /// Initialize against `provider`, resolving model/effort/tools from the
    /// configuration. A missing model is valid so the interactive frontend
    /// can prompt for one.
    pub fn new(provider: &dyn Provider, config: &Config, raw_mode: bool) -> AgentSession {
        let model = config.str("model").or_else(|| provider.default_model());
        let tools = if raw_mode {
            Vec::new()
        } else {
            crate::tools::ToolRegistry::new(crate::tools::ToolEnv {
                cwd: std::env::current_dir().unwrap_or_default(),
                config: config.clone(),
            })
            .defs()
        };
        let mut session = AgentSession {
            provider_id: provider.id().to_string(),
            provider_label: provider.display_name().to_string(),
            model_label: model.as_deref().map(|m| provider.model_label(m)),
            effort: resolve_effort(provider, config, model.as_deref()),
            model,
            system_prompt: String::new(),
            raw_mode,
            tools,
            items: Vec::new(),
            retired: Vec::new(),
            worked_ms: 0,
            last_user_turn_ms: -1,
            session_id: uuid::Uuid::new_v4().to_string(),
        };
        session.system_prompt = env::build_system_prompt(config);
        session
    }

    /// Re-resolve request settings for `provider` without changing history
    /// or tools.
    pub fn reconfigure(&mut self, provider: &dyn Provider, config: &Config) {
        self.provider_id = provider.id().to_string();
        self.provider_label = provider.display_name().to_string();
        if self.model.is_none() {
            self.model = provider.default_model();
        }
        if let Some(model) = &self.model {
            self.model_label = Some(provider.model_label(model));
        }
        if self.effort.is_none() {
            self.effort = resolve_effort(provider, config, self.model.as_deref());
        }
        if self.system_prompt.is_empty() {
            self.system_prompt = env::build_system_prompt(config);
        }
    }

    /// The provider context for the next request: items from the newest
    /// compaction seed onward (compaction summarizes a prefix rather than
    /// discarding it, so the model never re-pays for it).
    pub fn context(&self) -> Context<'_> {
        let floor = items_context_floor(&self.items);
        Context {
            system_prompt: Some(self.system_prompt.as_str()),
            items: &self.items[floor.min(self.items.len())..],
            tools: &self.tools,
            effort: self.effort.as_deref(),
            image_input: true,
            session_id: &self.session_id,
        }
    }

    pub fn add_user(&mut self, text: impl Into<String>) {
        self.items.push(Item::User {
            text: text.into(),
            images: Vec::new(),
            origin: ItemOrigin::None,
        });
    }

    /// Append the synthetic user turn used to resume an interrupted response.
    pub fn add_continuation(&mut self) {
        self.add_boundary();
        self.items.push(Item::User {
            text: CONTINUE_MARKER.to_string(),
            images: Vec::new(),
            origin: ItemOrigin::Continuation,
        });
    }

    /// Append a boundary between provider round-trips in one user turn.
    pub fn add_boundary(&mut self) {
        self.items.push(Item::Boundary);
    }

    /// Append the usage footer for one provider round-trip.
    pub fn add_turn_usage(
        &mut self,
        usage: &StreamUsage,
        elapsed_ms: i64,
        served_model: Option<&str>,
        response_id: Option<&str>,
        origin: ItemOrigin,
    ) {
        let mut provenance = TurnProvenance {
            provider_label: Some(self.provider_label.clone()),
            model_label: self.model_label.clone(),
            effort: self.effort.clone(),
            ..Default::default()
        };
        if let Some(served) = served_model {
            if Some(served) != self.model.as_deref() {
                provenance.served_model = Some(served.to_string());
            }
        }
        if let Some(id) = response_id {
            provenance.response_id = Some(id.to_string());
        }
        self.items.push(Item::Usage(Box::new(TurnUsage {
            usage: *usage,
            elapsed_ms,
            provenance,
            origin,
            cost_total: usage.cost,
            cost_estimated: false,
        })));
    }

    /// Stamp the recorded tail as interrupted unless the latest content is
    /// already a marked tool result, so an empty-send resume appends a
    /// continuation instead of re-sending a severed turn.
    pub fn mark_interrupt(&mut self) {
        if let Some(Item::ToolResult { output, .. }) = self.items.last_mut() {
            if !output.contains(INTERRUPT_MARKER) {
                output.push_str(&format!("\n{INTERRUPT_MARKER}"));
            }
            return;
        }
        if let Some(Item::Assistant { origin, .. }) = self.items.last_mut() {
            *origin = ItemOrigin::Interrupted;
        }
    }

    /// How a continuation without new user input speaks for the tail.
    pub fn resume_tail(&self) -> ResumeTail {
        let Some(last_content) = self
            .items
            .iter()
            .rev()
            .find(|i| !matches!(i, Item::Boundary | Item::Usage(_)))
        else {
            return ResumeTail::Empty;
        };
        match last_content {
            Item::User { origin: ItemOrigin::None, .. } => ResumeTail::User,
            Item::User { .. } => ResumeTail::Clean,
            Item::ToolResult { output, origin, .. } => {
                if output.contains(INTERRUPT_MARKER) || *origin == ItemOrigin::Interrupted {
                    ResumeTail::Marked
                } else {
                    ResumeTail::Clean
                }
            }
            Item::Assistant { origin, .. } => {
                if *origin == ItemOrigin::Interrupted {
                    ResumeTail::Marked
                } else {
                    ResumeTail::Clean
                }
            }
            _ => ResumeTail::Clean,
        }
    }

    /// Count typed user prompts; synthetic messages are excluded.
    pub fn user_turn_count(&self) -> usize {
        crate::item::items_user_turn_count(&self.items)
    }

    /// Prompt text for a zero-based typed user turn, or None out of range.
    pub fn user_turn_text(&self, index: usize) -> Option<String> {
        self.items
            .iter()
            .filter(|i| i.is_typed_prompt())
            .nth(index)
            .and_then(|item| match item {
                Item::User { text, .. } => Some(text.clone()),
                _ => None,
            })
    }

    /// Reset the conversation while preserving settings.
    pub fn reset(&mut self) {
        self.items.clear();
        self.retired.clear();
        self.worked_ms = 0;
        self.last_user_turn_ms = -1;
        self.session_id = uuid::Uuid::new_v4().to_string();
    }

    /// Move items from `from` to the retired list.
    pub fn retire(&mut self, from: usize) {
        self.retired.extend(self.items.drain(from..));
        self.last_user_turn_ms = -1;
    }

    /// Transfer completed turn items into the session.
    pub fn absorb(&mut self, sealed: Vec<Item>) {
        self.items.extend(sealed);
    }

    pub fn add_worked(&mut self, elapsed_ms: i64) {
        self.worked_ms += elapsed_ms;
        self.last_user_turn_ms = elapsed_ms;
    }

    /// Context-window size of the newest request a usage footer records in
    /// the model-visible context, or None when none reports it. Compaction's
    /// own accounting, which describes the summarized request rather than
    /// the fresh seed's window, is ignored.
    pub fn last_context_tokens(&self) -> Option<i64> {
        let floor = items_context_floor(&self.items);
        self.items[floor.min(self.items.len())..]
            .iter()
            .rev()
            .find_map(|item| match item {
                Item::Usage(usage) if usage.origin != ItemOrigin::Compaction => {
                    usage.usage.input_tokens
                }
                _ => None,
            })
    }
}

/// How a continuation without new user input speaks for the recorded tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeTail {
    /// No conversation to continue.
    Empty,
    /// Abort repair stamped the tail: append a continuation message.
    Marked,
    /// Unanswered user message: re-send history as recorded.
    User,
    /// Finished seam: continue it verbatim, owing a turn boundary.
    Clean,
}

/// Resolve the effective effort: config, then provider default, then the
/// provider's accepted set intersected with the requested value.
fn resolve_effort(
    provider: &dyn Provider,
    config: &Config,
    model: Option<&str>,
) -> Option<String> {
    let requested = config.str("effort");
    let accepted = model
        .map(|m| provider.list_efforts(m))
        .unwrap_or_default();
    match requested {
        Some(requested) => {
            if accepted.is_empty() {
                None
            } else {
                let set =
                    crate::effort::EffortSet::from_iter(accepted.iter().map(String::as_str));
                set.clamp(&requested).map(String::from)
            }
        }
        None => provider.default_effort(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_tail_classification() {
        let mut session = AgentSession {
            provider_id: "test".into(),
            provider_label: "Test".into(),
            model: None,
            model_label: None,
            effort: None,
            system_prompt: String::new(),
            raw_mode: false,
            tools: Vec::new(),
            items: Vec::new(),
            retired: Vec::new(),
            worked_ms: 0,
            last_user_turn_ms: -1,
            session_id: "s".into(),
        };
        assert_eq!(session.resume_tail(), ResumeTail::Empty);
        session.add_user("hello");
        assert_eq!(session.resume_tail(), ResumeTail::User);
        session.items.push(Item::assistant("answer"));
        assert_eq!(session.resume_tail(), ResumeTail::Clean);
        session.mark_interrupt();
        assert_eq!(session.resume_tail(), ResumeTail::Marked);
        session.add_continuation();
        assert!(matches!(session.items.last(), Some(Item::User {
            origin: ItemOrigin::Continuation,
            ..
        })));
    }

    #[test]
    fn usage_footer_records_provenance() {
        let mut session = AgentSession {
            provider_id: "test".into(),
            provider_label: "Test".into(),
            model: Some("m".into()),
            model_label: Some("m".into()),
            effort: Some("high".into()),
            system_prompt: String::new(),
            raw_mode: false,
            tools: Vec::new(),
            items: Vec::new(),
            retired: Vec::new(),
            worked_ms: 0,
            last_user_turn_ms: -1,
            session_id: "s".into(),
        };
        session.add_turn_usage(
            &StreamUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                ..Default::default()
            },
            1200,
            Some("m2"),
            Some("resp_1"),
            ItemOrigin::None,
        );
        match session.items.last() {
            Some(Item::Usage(usage)) => {
                assert_eq!(usage.usage.input_tokens, Some(10));
                assert_eq!(usage.elapsed_ms, 1200);
                assert_eq!(usage.provenance.effort.as_deref(), Some("high"));
                assert_eq!(usage.provenance.served_model.as_deref(), Some("m2"));
                assert_eq!(usage.provenance.response_id.as_deref(), Some("resp_1"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(session.last_context_tokens(), Some(10));
    }
}
