//! One-shot mode (`turnip -p "..."`): run a single agent turn with tools and
//! print the assistant's answer plainly on stdout, keeping stdout clean for
//! piping. `--json` prints the conversation as JSONL records instead.

use std::io::Write;

use turnip_core::agent::{self, TurnHooks, TurnOutcome};
use turnip_core::item::{Item, StreamUsage};
use turnip_core::provider::{AbortSignal, StreamEvent};
use turnip_core::session::SessionLog;
use turnip_core::tools::{ToolEnv, ToolRegistry};

use crate::bootstrap::App;

/// Hooks that print streamed text on stdout and tool noise on stderr;
/// JSON mode keeps stdout to records only.
struct OneshotHooks {
    json: bool,
}

impl TurnHooks for OneshotHooks {
    fn on_event(&mut self, event: &StreamEvent) -> bool {
        match event {
            StreamEvent::TextDelta { text } if !self.json => {
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(text.as_bytes());
                let _ = stdout.flush();
            }
            StreamEvent::ToolCallStart { name, .. } if !self.json => {
                let mut stderr = std::io::stderr();
                let _ = writeln!(stderr, "── {name}");
            }
            StreamEvent::Retry {
                attempt,
                max_attempts,
                delay_ms,
                ..
            } if !self.json => {
                let mut stderr = std::io::stderr();
                let _ = writeln!(
                    stderr,
                    "retrying (attempt {attempt}/{max_attempts}) in {delay_ms} ms"
                );
            }
            _ => {}
        }
        true
    }

    fn tick(&mut self) -> bool {
        true
    }
}

/// Whether session recording is on for this run (the `no_session` setting).
pub fn recording_enabled(config: &turnip_core::Config) -> bool {
    !matches!(config.tristate_or("no_session"), Some(true))
}

/// Run one prompt to completion. Returns the process exit code.
pub fn run(app: &mut App, prompt: String) -> i32 {
    let Some(provider) = app.provider.take() else {
        eprintln!("turnip: no provider available — set TURNIP_PROVIDER or configure one");
        return 1;
    };
    let mut session = app.build_session(provider.as_ref());
    if session.model.is_none() {
        eprintln!("turnip: no model selected — pass --model or set TURNIP_MODEL");
        return 1;
    }
    let items_baseline = session.items.len();
    session.add_user(prompt);

    let tools = ToolRegistry::new(ToolEnv {
        cwd: app.cwd.clone(),
        config: app.config.clone(),
    });
    let mut hooks = OneshotHooks { json: app.json };
    let signal = AbortSignal::new();
    let outcome = agent::run_user_turn(
        &mut session,
        provider.as_ref(),
        &tools,
        &app.config,
        &mut hooks,
        &signal,
    );

    // Record the turn in the session file, and print the resume hint on
    // stderr so stdout stays pipeable.
    let mut resume_hint = String::new();
    if recording_enabled(&app.config) {
        let log = match app.session_log.take() {
            Some(log) => Some(log),
            None => SessionLog::open(
                &session.provider_id,
                session.model.as_deref(),
                session.model_label.as_deref(),
                session.effort.as_deref(),
            ),
        };
        if let Some(mut log) = log {
            log.begin();
            log.append(&session.items);
            log.user_turn(session.last_user_turn_ms);
            log.flush();
            resume_hint = format!("turnip --resume {}", log.path().display());
            app.session_log = Some(log);
        }
    }

    if !app.json {
        let _ = std::io::stdout().write_all(b"\n");
    }

    match &outcome {
        TurnOutcome::Done | TurnOutcome::MaxTurns => {
            if app.json {
                print_json_records(&session, items_baseline);
            }
            if !resume_hint.is_empty() {
                eprintln!("resume with: {resume_hint}");
            }
            0
        }
        TurnOutcome::Interrupted => 130,
        TurnOutcome::Error(message) => {
            eprintln!("turnip: {message}");
            1
        }
    }
}

/// Print the conversation as JSONL: a session header, then every record.
fn print_json_records(session: &turnip_core::AgentSession, baseline: usize) {
    let header = serde_json::json!({
        "type": "session",
        "id": session.session_id,
        "cwd": std::env::current_dir().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default(),
        "provider": session.provider_id,
        "model": session.model,
        "effort": session.effort,
    });
    println!("{header}");
    for item in &session.items[baseline.min(session.items.len())..] {
        if let Ok(line) = serde_json::to_string(item) {
            println!("{line}");
        }
    }
    let totals = usage_totals(&session.items);
    let footer = serde_json::json!({
        "type": "usage_summary",
        "usage": totals,
        "worked_ms": session.worked_ms,
    });
    println!("{footer}");
}

/// Sum usage footers across the conversation.
pub fn usage_totals(items: &[Item]) -> StreamUsage {
    let mut totals = StreamUsage::default();
    for item in items {
        if let Item::Usage(usage) = item {
            totals.input_tokens = sum_opt(totals.input_tokens, usage.usage.input_tokens);
            totals.output_tokens = sum_opt(totals.output_tokens, usage.usage.output_tokens);
            totals.cost = sum_f64(totals.cost, usage.usage.cost);
        }
    }
    totals
}

fn sum_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, None) => a,
        (None, b) => b,
    }
}

fn sum_f64(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, None) => a,
        (None, b) => b,
    }
}
