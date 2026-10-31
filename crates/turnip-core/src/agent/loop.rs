//! The agent turn loop: stream a response, execute its tool calls, repeat
//! until the model answers without calls or a limit stops it.

use std::time::Instant;

use crate::config::Config;
use crate::item::{Item, ItemOrigin};
use crate::provider::{AbortSignal, Provider, StreamEvent, StreamObserver};
use crate::tools::{ToolIo, ToolRegistry, ToolSink};

use super::compact;
use super::turn::{PendingCall, TurnAssembly};
use super::AgentSession;

/// Why a turn stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    /// The model answered without tool calls.
    Done,
    /// The provider reported a fatal error.
    Error(String),
    /// The user interrupted the turn.
    Interrupted,
    /// The tool round-trip limit stopped the turn; an empty submit continues.
    MaxTurns,
}

/// Frontend hooks rendered during a turn.
pub trait TurnHooks {
    /// Observe a raw stream event (text deltas, reasoning, retries, errors).
    /// Return false to abort the stream.
    fn on_event(&mut self, _event: &StreamEvent) -> bool {
        true
    }
    /// A tool started: display its call line. Return false to abort.
    fn on_tool_start(&mut self, _call: &PendingCallView) -> bool {
        true
    }
    /// Live output bytes from the running tool.
    fn on_tool_output(&mut self, _bytes: &[u8]) {}
    /// Display-only note for the tool call (e.g. a diff preview).
    fn on_tool_note(&mut self, _text: &str) {}
    /// A tool finished; `output` is its model-facing result.
    fn on_tool_end(&mut self, _call: &PendingCallView, _output: &str) {}
    /// Pump UI events while the turn waits. Return false to interrupt.
    fn tick(&mut self) -> bool {
        true
    }
    /// Automatic compaction started (true) or finished successfully (false).
    fn on_compaction(&mut self, _started: bool) {}
}

/// Read-only view of a pending tool call for display.
pub struct PendingCallView {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// One complete user turn: streaming, tool execution, and multi-round-trip
/// continuation, with usage and timing recorded into the session.
///
/// The caller appends the user prompt (or nothing for an empty-send
/// continuation) and passes the session; on return the conversation holds
/// everything the turn produced.
pub fn run_user_turn(
    session: &mut AgentSession,
    provider: &dyn Provider,
    tools: &ToolRegistry,
    config: &Config,
    hooks: &mut dyn TurnHooks,
    signal: &AbortSignal,
) -> TurnOutcome {
    let started = Instant::now();
    let max_turns = config.int_or("max_turns", 0).max(0);
    let Some(model) = session.model.clone() else {
        return TurnOutcome::Error("no model selected".into());
    };

    let outcome = round_trip_loop(session, provider, tools, config, hooks, signal, &model, max_turns);

    // Record the wall time of the completed user turn.
    session.add_worked(started.elapsed().as_millis() as i64);
    outcome
}

fn round_trip_loop(
    session: &mut AgentSession,
    provider: &dyn Provider,
    tools: &ToolRegistry,
    config: &Config,
    hooks: &mut dyn TurnHooks,
    signal: &AbortSignal,
    model: &str,
    max_turns: i64,
) -> TurnOutcome {
    let mut round: i64 = 0;
    loop {
        // Compact automatically before a request that would overflow the
        // context window, so the model keeps answering instead of failing.
        if config.bool_or("compact.auto", true) && round == 0 {
            if let Err(error) = maybe_autocompact(session, provider, config, hooks, signal) {
                hooks.on_compaction(false);
                return TurnOutcome::Error(error);
            }
        }

        session.add_boundary();
        round += 1;
        let mut assembly = TurnAssembly::new();
        let stream_started = Instant::now();

        let result = {
            let mut sink = TurnSink {
                hooks,
                assembly: &mut assembly,
                provider: session.provider_id.as_str(),
                model,
                signal,
            };
            provider.stream(&session.context(), model, &mut sink)
        };
        {
            match result {
                Err(crate::CoreError::Aborted) => {
                    let elapsed = stream_started.elapsed().as_millis() as i64;
                    assembly.abort_repair();
                    let sealed = assembly.seal();
                    session.absorb(sealed);
                    if let Some(usage) = assembly.usage.as_ref() {
                        session.add_turn_usage(usage, elapsed, None, None, ItemOrigin::None);
                    }
                    session.mark_interrupt();
                    return TurnOutcome::Interrupted;
                }
                Err(error) => {
                    assembly.abort_repair();
                    let sealed = assembly.seal();
                    session.absorb(sealed);
                    session.mark_interrupt();
                    return TurnOutcome::Error(error.to_string());
                }
                Ok(()) => {}
            }
        }

        if let Some(message) = assembly.error_message.clone() {
            // A provider error keeps only assistant text: a call that never
            // ran would need a fabricated result, and with both dropped a
            // retry re-issues the same request.
            assembly.abort_repair();
            let sealed = assembly.seal();
            session.absorb(sealed);
            if let Some(usage) = &assembly.usage {
                session.add_turn_usage(
                    usage,
                    stream_started.elapsed().as_millis() as i64,
                    None,
                    None,
                    ItemOrigin::None,
                );
            }
            session.mark_interrupt();
            return TurnOutcome::Error(message.clone());
        }

        // Completed stream: seal reasoning, text, and calls.
        let sealed = assembly.seal();
        let has_calls = sealed.iter().any(|i| matches!(i, Item::ToolCall { .. }));
        session.absorb(sealed);
        if let Some(usage) = &assembly.usage {
            session.add_turn_usage(
                usage,
                stream_started.elapsed().as_millis() as i64,
                None,
                None,
                ItemOrigin::None,
            );
        }
        if !has_calls {
            return TurnOutcome::Done;
        }

        // Collect this round trip's calls, then execute and continue.
        let calls: Vec<PendingCall> = session
            .items
            .iter()
            .rev()
            .take_while(|i| !matches!(i, Item::Boundary))
            .filter_map(|i| match i {
                Item::ToolCall {
                    call_id,
                    tool_name,
                    arguments,
                } => Some(PendingCall {
                    id: call_id.clone(),
                    name: tool_name.clone(),
                    arguments: arguments.clone(),
                }),
                _ => None,
            })
            .collect();
        execute_calls(session, tools, hooks, signal, calls);

        if max_turns > 0 && round >= max_turns {
            return TurnOutcome::MaxTurns;
        }
    }
}

fn execute_calls(
    session: &mut AgentSession,
    tools: &ToolRegistry,
    hooks: &mut dyn TurnHooks,
    signal: &AbortSignal,
    calls: Vec<PendingCall>,
) {
    for call in calls {
        let view = PendingCallView {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        };
        if !hooks.on_tool_start(&view) || signal.is_aborted() {
            session.items.push(Item::ToolResult {
                call_id: call.id,
                output: super::INTERRUPT_MARKER.to_string(),
                hidden_tail: 0,
                images: Vec::new(),
                origin: ItemOrigin::Skipped,
            });
            break;
        }
        if session.raw_mode {
            hooks.on_tool_end(&view, super::REFUSED_RESULT);
            session.items.push(Item::ToolResult {
                call_id: call.id,
                output: super::REFUSED_RESULT.to_string(),
                hidden_tail: 0,
                images: Vec::new(),
                origin: ItemOrigin::Refused,
            });
            continue;
        }
        let mut io = ToolIo {
            sink: Some(&mut HooksToolSink { hooks, signal }),
            image_input: true,
            images: Vec::new(),
            hidden_tail: 0,
            interrupted: false,
        };
        let output = tools.dispatch(&call.name, &call.arguments, &mut io);
        let images = std::mem::take(&mut io.images);
        let hidden_tail = io.hidden_tail;
        let interrupted = io.interrupted;
        drop(io);
        let call_id = call.id.clone();
        hooks.on_tool_end(&view, &output);
        session.items.push(Item::ToolResult {
            call_id,
            output,
            hidden_tail,
            images,
            origin: if interrupted {
                ItemOrigin::Interrupted
            } else {
                ItemOrigin::None
            },
        });
        if interrupted || signal.is_aborted() {
            break;
        }
    }
}

/// Feeds stream events into both the assembly and the frontend hooks.
struct TurnSink<'a> {
    hooks: &'a mut dyn TurnHooks,
    assembly: &'a mut TurnAssembly,
    provider: &'a str,
    model: &'a str,
    signal: &'a AbortSignal,
}

impl<'a> StreamObserver for TurnSink<'a> {
    fn on_event(&mut self, event: StreamEvent) -> bool {
        self.assembly.consume(&event, self.provider, self.model);
        if self.signal.is_aborted() {
            return false;
        }
        self.hooks.on_event(&event)
    }

    fn tick(&mut self) -> bool {
        !self.signal.is_aborted() && self.hooks.tick()
    }
}

/// Adapts [`TurnHooks`] to the tool sink interface.
struct HooksToolSink<'a> {
    hooks: &'a mut dyn TurnHooks,
    signal: &'a AbortSignal,
}

impl<'a> ToolSink for HooksToolSink<'a> {
    fn display(&mut self, bytes: &[u8]) {
        self.hooks.on_tool_output(bytes);
    }

    fn note(&mut self, text: &str) {
        self.hooks.on_tool_note(text);
    }

    fn tick(&mut self) -> bool {
        !self.signal.is_aborted() && self.hooks.tick()
    }
}

/// Compact before sending when the recorded context crossed the configured
/// fraction of the model's window. Unknown windows never compact.
fn maybe_autocompact(
    session: &mut AgentSession,
    provider: &dyn Provider,
    config: &Config,
    hooks: &mut dyn TurnHooks,
    signal: &AbortSignal,
) -> Result<(), String> {
    let Some(model) = session.model.clone() else {
        return Ok(());
    };
    let threshold = config.int_or("compact.threshold", 85).clamp(1, 100);
    let context_window = config
        .tokens_or("context_limit", 0)
        .max(provider.model_info(&model).map(|i| i.context_window).unwrap_or(0));
    if context_window <= 0 {
        return Ok(());
    }
    let Some(last_input) = session.last_context_tokens() else {
        return Ok(());
    };
    if last_input * 100 < context_window * threshold {
        return Ok(());
    }
    hooks.on_compaction(true);
    match compact::run(session, provider, config, hooks, signal, None, true) {
        Ok(true) => {
            hooks.on_compaction(false);
            Ok(())
        }
        Ok(false) => Ok(()),
        Err(error) => Err(error),
    }
}
