//! Conversation compaction: replace the history with a summarization seed
//! so long conversations keep fitting the context window.

use crate::config::Config;
use crate::item::{Item, ItemOrigin, StreamUsage, TurnUsage};
use crate::provider::{AbortSignal, Context, Provider, StreamEvent, StreamObserver};

use super::r#loop::TurnHooks;
use super::AgentSession;

const SUMMARY_INSTRUCTIONS: &str = "You are summarizing a conversation between a user and a \
coding assistant so it can continue with a fresh context. Write a dense, factual summary in \
plain text (not markdown headings) that keeps: the user's goals and constraints, decisions made \
and their reasons, files touched and their current state, the latest instructions, and any \
unresolved problem in progress. Preserve exact paths, identifiers, and commands. Omit \
pleasantries and restating the tool plumbing. The summary replaces the history, so everything \
the next response needs must appear in it.";

/// Replace the conversation with a streamed summary. Returns Ok(true) when
/// history was replaced, Ok(false) when there was nothing to summarize, or
/// Err with a user-actionable message (leaving the session unchanged).
pub fn run(
    session: &mut AgentSession,
    provider: &dyn Provider,
    config: &Config,
    hooks: &mut dyn TurnHooks,
    signal: &AbortSignal,
    instructions: Option<&str>,
    automatic: bool,
) -> Result<bool, String> {
    let _ = automatic;
    let Some(model) = session.model.clone() else {
        return Err("no model selected".into());
    };
    if session.items.is_empty() {
        return Ok(false);
    }
    let system = match instructions {
        Some(text) if !text.trim().is_empty() => format!("{SUMMARY_INSTRUCTIONS}\n\n{text}"),
        _ => SUMMARY_INSTRUCTIONS.to_string(),
    };
    let context = Context {
        system_prompt: Some(&system),
        items: &session.items,
        tools: &[],
        effort: session.effort.as_deref(),
        image_input: false,
        session_id: &session.session_id,
    };

    let mut collector = SummaryCollector {
        text: String::new(),
        usage: None,
        error: None,
        hooks,
        signal,
    };
    let _ = config;
    let result = provider.stream(&context, &model, &mut collector);
    match result {
        Err(crate::CoreError::Aborted) => return Err("interrupted".into()),
        Err(error) => return Err(error.to_string()),
        Ok(()) => {}
    }
    if let Some(error) = collector.error {
        return Err(error);
    }
    let summary = collector.text.trim();
    if summary.is_empty() {
        return Err("the model returned an empty summary".into());
    }

    // Everything before the seed is summarized away but stays in `retired`
    // for accounting; the compaction footer records the summarization
    // request's own usage, outside the conversation's window.
    let usage = collector.usage.unwrap_or_default();
    session.retired.append(&mut session.items);
    session.items.push(Item::Usage(Box::new(TurnUsage {
        usage,
        elapsed_ms: 0,
        provenance: Default::default(),
        origin: ItemOrigin::Compaction,
        cost_total: usage.cost,
        cost_estimated: false,
    })));
    session.items.push(Item::User {
        text: format!(
            "Summary of the conversation so far, which was compacted to fit the context \
             window:\n\n{summary}"
        ),
        images: Vec::new(),
        origin: ItemOrigin::CompactSeed,
    });
    session.last_user_turn_ms = -1;
    Ok(true)
}

/// Collects the summary stream: text and usage are consumed internally;
/// only retry/error events and the tick reach the frontend.
struct SummaryCollector<'a> {
    text: String,
    usage: Option<StreamUsage>,
    error: Option<String>,
    hooks: &'a mut dyn TurnHooks,
    signal: &'a AbortSignal,
}

impl<'a> StreamObserver for SummaryCollector<'a> {
    fn on_event(&mut self, event: StreamEvent) -> bool {
        match event {
            StreamEvent::TextDelta { text } => {
                self.text.push_str(&text);
                true
            }
            StreamEvent::Done { usage, .. } => {
                self.usage
                    .get_or_insert_with(StreamUsage::empty)
                    .absorb(&usage);
                true
            }
            StreamEvent::Error { message, usage, .. } => {
                if let Some(usage) = usage {
                    self.usage
                        .get_or_insert_with(StreamUsage::empty)
                        .absorb(&usage);
                }
                self.error.get_or_insert(message);
                false
            }
            other => self.hooks.on_event(&other),
        }
    }

    fn tick(&mut self) -> bool {
        !self.signal.is_aborted() && self.hooks.tick()
    }
}
