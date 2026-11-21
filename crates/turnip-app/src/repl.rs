//! The interactive REPL: input editing, slash commands, streaming rendering
//! with a live region over native scrollback, pickers, and session
//! management.

use std::path::Path;
use std::time::{Duration, Instant};

use turnip_core::agent::{self, compact, TurnHooks, TurnOutcome};
use turnip_core::item::{items_user_turn_cut, StreamUsage};
use turnip_core::provider::{AbortSignal, Provider, ProviderDescriptor, StreamEvent};
use turnip_core::session::{self, SessionLog};
use turnip_core::tools::{ToolEnv, ToolRegistry};
use turnip_core::{Config, AgentSession};

use crate::bootstrap::App;
use crate::term::{markdown, picker, LineEditor, LineStyle, StyledLine, Terminal, PROMPT};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The stream channels that write to the scrollback during a turn. Blocks
/// of different kinds are separated by exactly one blank line, whatever
/// the chunk boundaries cut them at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    Assistant,
    Reasoning,
    Tool,
}

/// The interactive frontend.
pub struct Repl {
    terminal: Terminal,
    editor: LineEditor,
    provider: Box<dyn Provider>,
    session: AgentSession,
    tools: ToolRegistry,
    config: Config,
    cwd: std::path::PathBuf,
    session_log: Option<SessionLog>,
    signal: AbortSignal,
    state: RenderState,
    /// Prompt submitted while a turn was running; sent right after it.
    queued_prompt: Option<String>,
    quit: bool,
    markdown: bool,
    show_reasoning: bool,
}

/// Streaming render state, reused across turns.
struct RenderState {
    stream_text: String,
    flushed_chars: usize,
    reasoning_text: String,
    reasoning_flushed: usize,
    tool: Option<ToolView>,
    status_note: Option<String>,
    turn_started: Instant,
    turn_usage: Option<StreamUsage>,
    spinner: usize,
    interrupted: bool,
    last_render: Option<Instant>,
    /// Channel of the block that wrote to the scrollback last, and whether
    /// it ended on a blank line.
    last_channel: Option<Channel>,
    last_blank: bool,
}

struct ToolView {
    name: String,
    argument: String,
    output: String,
}

impl RenderState {
    fn new() -> RenderState {
        RenderState {
            stream_text: String::new(),
            flushed_chars: 0,
            reasoning_text: String::new(),
            reasoning_flushed: 0,
            tool: None,
            status_note: None,
            turn_started: Instant::now(),
            turn_usage: None,
            spinner: 0,
            interrupted: false,
            last_render: None,
            last_channel: None,
            last_blank: false,
        }
    }

    fn reset_turn(&mut self) {
        self.stream_text.clear();
        self.flushed_chars = 0;
        self.reasoning_text.clear();
        self.reasoning_flushed = 0;
        self.tool = None;
        self.status_note = None;
        self.turn_usage = None;
        self.interrupted = false;
        self.turn_started = Instant::now();
        self.last_render = None;
        self.last_channel = None;
        self.last_blank = false;
    }
}

impl Repl {
    /// Build the REPL, resolving provider and model through pickers when
    /// configuration leaves them unset.
    pub fn start(mut app: App) -> Result<Repl, String> {
        let mut terminal =
            Terminal::new().map_err(|e| format!("cannot enter raw mode: {e}"))?;

        // Resolve the provider interactively when nothing was selected.
        let provider = match app.provider.take() {
            Some(provider) => provider,
            None => {
                let transport =
                    turnip_core::provider::registry::transport_from_config(&app.config);
                let mut available = Vec::new();
                for descriptor in turnip_core::provider::registry::descriptors(&app.config) {
                    let status = turnip_core::provider::registry::availability(
                        &app.config,
                        &descriptor,
                        &transport,
                    );
                    available.push(picker::PickerItem {
                        id: descriptor.id.clone(),
                        label: descriptor.display_name.clone(),
                        detail: status.err().map(|reason| reason),
                    });
                }
                if available.is_empty() {
                    return Err("no providers configured".into());
                }
                let Some(id) = picker::pick(
                    &mut terminal,
                    "Choose a provider",
                    available,
                    "",
                )
                .ok()
                .flatten()
                else {
                    return Err("no provider selected".into());
                };
                app.config.set_run("provider", Some(&id));
                app.select_provider();
                app.provider
                    .take()
                    .ok_or_else(|| format!("cannot construct provider '{id}'"))?
            }
        };
        print_banner(&mut terminal, &app);

        let cwd = app.cwd.clone();
        let config = app.config.clone();
        let history = session::history_read(&cwd);

        let session = app.build_session(provider.as_ref());
        let mut repl = Repl {
            terminal,
            editor: LineEditor::new(history),
            provider,
            session,
            tools: ToolRegistry::new(ToolEnv {
                cwd: cwd.clone(),
                config: config.clone(),
            }),
            config,
            cwd,
            session_log: app.session_log.take(),
            signal: AbortSignal::new(),
            state: RenderState::new(),
            queued_prompt: None,
            quit: false,
            markdown: app.config.bool_or("markdown", true),
            show_reasoning: app.config.bool_or("show_reasoning", false),
        };

        // Resolve the model through a picker when still unset.
        if repl.session.model.is_none() {
            repl.pick_model()?;
        }
        Ok(repl)
    }

    pub fn run(mut self) -> i32 {
        let code = self.main_loop();
        // Final flush of the session log.
        if let Some(log) = &mut self.session_log {
            log.flush();
        }
        code
    }

    fn main_loop(&mut self) -> i32 {
        self.render_idle();
        while !self.quit {
            match self.terminal.wait_event(crate::term::POLL_INTERVAL) {
                Ok(Some(crate::term::TermEvent::Key(key))) => {
                    if self.editor.handle_key(&key, &mut self.terminal).unwrap_or(false) {
                        if self.editor.quit_requested {
                            return 0;
                        }
                        let line = self.editor.take();
                        if !self.submit(line) {
                            return 0;
                        }
                    }
                }
                Ok(Some(crate::term::TermEvent::Paste(text))) => self.editor.insert_str(&text),
                Ok(Some(crate::term::TermEvent::Resize)) => {}
                // Nothing changed; do not repaint on poll timeouts.
                Ok(None) => continue,
                Err(_) => return 1,
            }
            while let Some(prompt) = self.queued_prompt.take() {
                // A prompt was typed during the previous turn.
                self.submit_prompt(prompt);
            }
            self.render_idle();
        }
        0
    }

    /// Handle a submitted line. Returns false when the REPL should exit.
    fn submit(&mut self, line: String) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Empty submit: continue an interrupted or capped turn.
            match self.session.resume_tail() {
                turnip_core::agent::ResumeTail::Empty => {}
                _ => self.run_turn(),
            }
            return !self.quit;
        }
        if let Some(command) = trimmed.strip_prefix('/') {
            return self.slash(command.trim());
        }
        if trimmed == "quit" || trimmed == "exit" {
            return false;
        }
        self.submit_prompt(line);
        !self.quit
    }

    fn submit_prompt(&mut self, prompt: String) {
        let expanded = expand_mentions(&prompt, &self.cwd);
        self.session.add_user(expanded.clone());
        if let Some(log) = &mut self.session_log {
            log.append(&self.session.items);
        }
        let history = session::history_append(&self.cwd, &expanded);
        self.editor.history = history;
        // Echo the turn into the scrollback — one separating blank line,
        // then the prompt marked like the input line — so user turns stay
        // visible and read apart from the assistant stream, tool blocks,
        // and reasoning that follow.
        let accent = LineStyle {
            accent: true,
            ..Default::default()
        };
        let mut echo = vec![StyledLine::plain("")];
        for (index, line) in expanded.split('\n').enumerate() {
            let marker = if index == 0 { PROMPT } else { "  " };
            echo.push(StyledLine::styled(format!("{marker}{line}"), accent));
        }
        let _ = self.terminal.push_lines(&echo);
        self.run_turn();
    }

    /// Run one agent turn with live rendering.
    fn run_turn(&mut self) {
        self.state.reset_turn();
        let _ = self.terminal.hide_cursor();
        let (markdown, show_reasoning) = (self.markdown, self.show_reasoning);

        let outcome = {
            let Repl {
                provider,
                tools,
                config,
                signal,
                session,
                terminal,
                editor,
                state,
                queued_prompt,
                ..
            } = self;
            let mut renderer = TurnRenderer {
                terminal,
                editor,
                state,
                signal,
                queued_prompt,
                markdown,
                show_reasoning,
            };
            let outcome = agent::run_user_turn(
                session,
                provider.as_ref(),
                tools,
                config,
                &mut renderer,
                signal,
            );
            renderer.finish(outcome)
        };

        let _ = self.terminal.show_cursor();

        match outcome {
            TurnOutcome::Done | TurnOutcome::MaxTurns => {
                if matches!(outcome, TurnOutcome::MaxTurns) {
                    self.push_note("── tool round-trip limit reached; submit an empty line to continue ──");
                }
            }
            TurnOutcome::Interrupted => {
                self.push_note("(interrupted)");
            }
            TurnOutcome::Error(message) => {
                self.push_line(StyledLine::styled(message, LineStyle { error: true, ..Default::default() }));
            }
        }

        if let Some(log) = &mut self.session_log {
            log.append(&self.session.items);
            log.user_turn(self.session.last_user_turn_ms);
            log.flush();
        }
    }

    /// Push a finished line into the scrollback and redraw the input area.
    fn push_line(&mut self, line: StyledLine) {
        let _ = self.terminal.push_lines(&[line]);
    }

    fn push_note(&mut self, text: &str) {
        self.push_line(StyledLine::styled(text, LineStyle { dim: true, ..Default::default() }));
    }

    /// Render the idle input region: prompt line(s) plus footer.
    fn render_idle(&mut self) {
        let (width, height) = self.terminal.size();
        let (mut lines, (row, column)) = self.editor.view(width, height.saturating_sub(1));
        if height > 1 {
            lines.push(self.footer_line(width));
        }
        let _ = self.terminal.render_region(&lines);
        let _ = self.terminal.cursor_in_region(row as u16, column as u16);
    }

    fn footer_line(&self, width: usize) -> StyledLine {
        let model = self
            .session
            .model_label
            .clone()
            .or_else(|| self.session.model.clone())
            .unwrap_or_default();
        let effort = self
            .session
            .effort
            .as_deref()
            .map(|e| format!(" · {e}"))
            .unwrap_or_default();
        let totals = App::usage_totals(&self.session.items);
        let tokens = format_tokens(&totals);
        let left = format!("{} · {model}{effort}", self.session.provider_label);
        let right = if tokens.is_empty() {
            String::new()
        } else {
            format!("  {tokens}")
        };
        let hint = "  /help for commands";
        let mut text = format!("{left}{right}");
        let room = width.saturating_sub(turnip_core::text::str_width(&text) + hint.len());
        if room > 2 {
            text.push_str(&" ".repeat(room));
            text.push_str(hint);
        }
        StyledLine::styled(
            turnip_core::text::truncate_columns(
                &turnip_core::text::sanitize_str(&text).replace('\n', " "),
                width,
            ),
            LineStyle {
                dim: true,
                ..Default::default()
            },
        )
    }

    /// Interactive model resolution.
    fn pick_model(&mut self) -> Result<(), String> {
        let models = self.provider.list_models().unwrap_or_default();
        if models.is_empty() {
            return Err("no models available".into());
        }
        let mut sort = self.config.tristate_or("sort_models");
        if sort.is_none() {
            sort = Some(true);
        }
        let mut models = models;
        if sort == Some(true) {
            models.sort_by(|a, b| turnip_core::model::compare_model_ids(&a.id, &b.id));
        }
        let items: Vec<picker::PickerItem> = models
            .iter()
            .map(|model| picker::PickerItem {
                id: model.id.clone(),
                label: model.id.clone(),
                detail: model
                    .description
                    .clone()
                    .map(|d| turnip_core::text::truncate_columns(&d, 48)),
            })
            .collect();
        let model = picker::pick(&mut self.terminal, "Choose a model", items, "")
            .ok()
            .flatten()
            .ok_or("no model selected")?;
        self.set_model(Some(model));
        Ok(())
    }

    fn set_model(&mut self, model: Option<String>) {
        self.session.model = model.clone();
        self.session.model_label = model.as_deref().map(|m| self.provider.model_label(m));
        if let Some(model) = &model {
            self.config.set_run("model", Some(model));
        }
        let _ = self.config.persist_selection(
            &self.session.provider_id,
            model.as_deref(),
            self.session.effort.as_deref(),
        );
        if let Some(log) = &mut self.session_log {
            log.set_meta(
                Some(&self.session.provider_id),
                self.session.model.as_deref(),
                self.session.model_label.as_deref(),
                self.session.effort.as_deref(),
            );
        }
    }

    // ---- Slash commands -------------------------------------------------

    /// Dispatch a slash command; returns false when the REPL should exit.
    fn slash(&mut self, command: &str) -> bool {
        let (name, rest) = match command.split_once(' ') {
            Some((name, rest)) => (name, rest.trim()),
            None => (command, ""),
        };
        match name {
            "quit" | "exit" => return false,
            "help" => self.cmd_help(),
            "model" => self.cmd_model(rest),
            "provider" => self.cmd_provider(rest),
            "effort" => self.cmd_effort(rest),
            "new" => self.cmd_new(),
            "clear" => {
                let _ = self.terminal.clear();
                self.state.last_render = None;
            }
            "compact" => self.cmd_compact(rest),
            "undo" => self.cmd_undo(rest),
            "resume" => self.cmd_resume(rest),
            "fork" => self.cmd_fork(),
            "usage" => self.cmd_usage(),
            "config" => self.cmd_config(rest),
            "session" => self.cmd_session(),
            other => self.push_line(StyledLine::styled(
                format!("unknown command /{other} — try /help"),
                LineStyle { error: true, ..Default::default() },
            )),
        }
        !self.quit
    }

    fn cmd_help(&mut self) {
        let help = [
            ("/help", "show this help"),
            ("/model [id]", "pick or switch the model"),
            ("/provider [id]", "pick or switch the provider"),
            ("/effort [level]", "pick the reasoning effort (or off)"),
            ("/new", "start a fresh conversation"),
            ("/resume", "pick a recorded session for this directory"),
            ("/fork", "branch the conversation into a new session file"),
            ("/undo [n]", "revert the last n user turns (default 1)"),
            ("/compact [note]", "summarize the conversation to free context"),
            ("/usage", "session token and cost totals"),
            ("/session", "show the session file and its resume command"),
            ("/config [key [value]]", "show or set a configuration value"),
            ("/clear", "clear the screen"),
            ("/quit", "exit"),
            ("", "submit an empty line to continue an interrupted turn"),
            ("Ctrl+C", "interrupt the running turn; clear the input"),
        ];
        self.push_note("commands:");
        for (name, description) in help {
            self.push_line(StyledLine::styled(
                format!("  {name:<24} {description}"),
                LineStyle {
                    dim: true,
                    ..Default::default()
                },
            ));
        }
    }

    fn cmd_model(&mut self, rest: &str) {
        if !rest.is_empty() {
            self.set_model(Some(rest.to_string()));
            let label = self
                .session
                .model_label
                .clone()
                .unwrap_or_else(|| rest.to_string());
            self.push_note(&format!("model: {label}"));
            return;
        }
        match self.pick_model() {
            Ok(()) => {
                let label = self
                    .session
                    .model_label
                    .clone()
                    .or_else(|| self.session.model.clone())
                    .unwrap_or_default();
                self.push_note(&format!("model: {label}"));
            }
            Err(error) => self.push_line(StyledLine::styled(
                error,
                LineStyle { error: true, ..Default::default() },
            )),
        }
    }

    fn cmd_provider(&mut self, rest: &str) {
        let transport = turnip_core::provider::registry::transport_from_config(&self.config);
        let descriptors = turnip_core::provider::registry::descriptors(&self.config);
        let descriptor: Option<ProviderDescriptor> = if rest.is_empty() {
            let mut items = Vec::new();
            for candidate in &descriptors {
                let status = turnip_core::provider::registry::availability(
                    &self.config,
                    candidate,
                    &transport,
                );
                items.push(picker::PickerItem {
                    id: candidate.id.clone(),
                    label: candidate.display_name.clone(),
                    detail: status.err().map(|reason| reason),
                });
            }
            if items.is_empty() {
                self.push_line(StyledLine::styled(
                    "no providers configured",
                    LineStyle { error: true, ..Default::default() },
                ));
                return;
            }
            let active = self.session.provider_id.clone();
            picker::pick(&mut self.terminal, "Choose a provider", items, &active)
                .ok()
                .flatten()
                .and_then(|id| descriptors.into_iter().find(|d| d.id == id))
        } else {
            descriptors.into_iter().find(|d| d.id == rest)
        };
        let Some(descriptor) = descriptor else {
            return;
        };
        self.config.set_run("provider", Some(&descriptor.id));
        let transport = turnip_core::provider::registry::transport_from_config(&self.config);
        let Some(built) =
            turnip_core::provider::registry::build(&self.config, &descriptor, transport)
        else {
            self.push_line(StyledLine::styled(
                format!("cannot construct provider '{}'", descriptor.id),
                LineStyle { error: true, ..Default::default() },
            ));
            return;
        };
        self.provider = built;
        self.session.reconfigure(self.provider.as_ref(), &self.config);
        if self.session.model.is_none() {
            if self.provider.is_local() {
                if let Ok(models) = self.provider.list_models() {
                    if let Some(first) = models.first() {
                        self.config.set_run("model", Some(&first.id));
                        self.session.reconfigure(self.provider.as_ref(), &self.config);
                    }
                }
            }
            if self.session.model.is_none() {
                let _ = self.pick_model();
            }
        }
        let _ = self.config.persist_selection(
            &self.session.provider_id,
            self.session.model.as_deref(),
            self.session.effort.as_deref(),
        );
        if let Some(log) = &mut self.session_log {
            log.set_meta(
                Some(&self.session.provider_id),
                self.session.model.as_deref(),
                self.session.model_label.as_deref(),
                self.session.effort.as_deref(),
            );
        }
        self.push_note(&format!(
            "provider: {} · model: {}",
            self.session.provider_label,
            self.session
                .model_label
                .clone()
                .or_else(|| self.session.model.clone())
                .unwrap_or_default()
        ));
    }

    fn cmd_effort(&mut self, rest: &str) {
        let model = self.session.model.clone().unwrap_or_default();
        let mut levels = self.provider.list_efforts(&model);
        if rest.is_empty() {
            if levels.is_empty() {
                self.push_note("this provider takes no reasoning effort");
                return;
            }
            let items: Vec<picker::PickerItem> = levels
                .iter()
                .chain(std::iter::once(&"off".to_string()))
                .map(|level| picker::PickerItem {
                    id: level.clone(),
                    label: level.clone(),
                    detail: None,
                })
                .collect();
            let Some(choice) = picker::pick(&mut self.terminal, "Reasoning effort", items, "")
                .ok()
                .flatten()
            else {
                return;
            };
            self.set_effort(if choice == "off" { None } else { Some(choice) });
        } else if rest == "off" {
            self.set_effort(None);
        } else {
            levels.clear();
            levels.push(rest.to_string());
            self.set_effort(Some(rest.to_string()));
        }
    }

    fn set_effort(&mut self, effort: Option<String>) {
        self.session.effort = effort;
        let _ = self.config.persist_selection(
            &self.session.provider_id,
            self.session.model.as_deref(),
            self.session.effort.as_deref(),
        );
        if let Some(log) = &mut self.session_log {
            log.set_meta(
                Some(&self.session.provider_id),
                self.session.model.as_deref(),
                self.session.model_label.as_deref(),
                self.session.effort.as_deref(),
            );
        }
        match &self.session.effort {
            Some(level) => self.push_note(&format!("effort: {level}")),
            None => self.push_note("effort: off"),
        }
    }

    fn cmd_new(&mut self) {
        self.session.reset();
        if let Some(log) = &mut self.session_log {
            log.reset(
                Some(&self.session.provider_id),
                self.session.model.as_deref(),
                self.session.effort.as_deref(),
            );
        }
        self.push_note("started a new conversation");
    }

    fn cmd_compact(&mut self, note: &str) {
        self.state.reset_turn();
        let instructions = if note.is_empty() { None } else { Some(note) };
        let (markdown, show_reasoning) = (self.markdown, self.show_reasoning);
        let result = {
            let Repl {
                provider,
                config,
                signal,
                session,
                terminal,
                editor,
                state,
                ..
            } = self;
            let mut renderer = TurnRenderer {
                terminal,
                editor,
                state,
                signal,
                queued_prompt: &mut None,
                markdown,
                show_reasoning,
            };
            compact::run(
                session,
                provider.as_ref(),
                config,
                &mut renderer,
                signal,
                instructions,
                false,
            )
        };
        match result {
            Ok(true) => {
                self.push_note("── compacted ──");
                if let Some(log) = &mut self.session_log {
                    log.append(&self.session.items);
                    log.flush();
                }
            }
            Ok(false) => self.push_note("nothing to compact"),
            Err(error) => self.push_line(StyledLine::styled(
                format!("compaction failed: {error}"),
                LineStyle { error: true, ..Default::default() },
            )),
        }
    }

    fn cmd_undo(&mut self, rest: &str) {
        let count = self.session.user_turn_count();
        let undo_count: usize = rest.parse().unwrap_or(1);
        if undo_count == 0 || undo_count > count {
            self.push_note(&format!("conversation has {count} user turn(s)"));
            return;
        }
        let keep = count - undo_count;
        let cut = items_user_turn_cut(&self.session.items, keep);
        if cut >= self.session.items.len() {
            self.push_note("nothing to undo");
            return;
        }
        self.session.retire(cut);
        if let Some(log) = &mut self.session_log {
            let _ = log.undo(keep, cut);
            log.append(&self.session.items);
            log.flush();
        }
        self.push_note(&format!(
            "undid {undo_count} turn(s); {} remain",
            self.session.user_turn_count()
        ));
    }

    fn cmd_resume(&mut self, rest: &str) {
        let path = if rest.is_empty() {
            let mut entries = session::list(&self.cwd);
            for entry in &mut entries {
                session::label_read(entry, 60);
            }
            if entries.is_empty() {
                self.push_note("no recorded sessions for this directory");
                return;
            }
            let items: Vec<picker::PickerItem> = entries
                .iter()
                .map(|entry| picker::PickerItem {
                    id: entry.path.to_string_lossy().into_owned(),
                    label: entry
                        .label
                        .prompt
                        .clone()
                        .unwrap_or_else(|| "(no prompt recorded)".into()),
                    detail: Some(format!(
                        "{}  {}{}",
                        session::format_mtime(entry.mtime_secs),
                        entry.label.model.clone().unwrap_or_default(),
                        entry
                            .label
                            .provider
                            .clone()
                            .map(|p| format!(" · {p}"))
                            .unwrap_or_default()
                    )),
                })
                .collect();
            match picker::pick(&mut self.terminal, "Resume a session", items, "") {
                Ok(Some(path)) => std::path::PathBuf::from(path),
                _ => return,
            }
        } else {
            std::path::PathBuf::from(rest)
        };
        let loaded = match session::load_all(&path) {
            Ok(loaded) => loaded,
            Err(error) => {
                self.push_line(StyledLine::styled(
                    error.to_string(),
                    LineStyle { error: true, ..Default::default() },
                ));
                return;
            }
        };
        self.session.items = loaded.items;
        self.session.retired = loaded.retired;
        self.session.worked_ms = loaded.worked_ms;
        self.session.last_user_turn_ms = loaded.last_user_turn_ms;
        self.session.session_id = loaded.meta.id.clone();
        self.session_log = SessionLog::resume(
            &path,
            &self.session.provider_id,
            self.session.model.as_deref(),
            self.session.model_label.as_deref(),
            self.session.effort.as_deref(),
            self.session.items.len(),
        );
        self.push_note(&format!(
            "resumed {} ({} user turn(s))",
            path.display(),
            self.session.user_turn_count()
        ));
    }

    fn cmd_fork(&mut self) {
        let Some(log) = &self.session_log else {
            self.push_note("session recording is off; nothing to fork");
            return;
        };
        let source = log.path().to_path_buf();
        let meta = turnip_core::session::SessionMeta {
            id: self.session.session_id.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            provider: Some(self.session.provider_id.clone()),
            model: self.session.model.clone(),
            model_label: self.session.model_label.clone(),
            effort: self.session.effort.clone(),
        };
        match session::fork_file(&source, &self.session.items, &meta) {
            Ok(path) => {
                self.session_log = SessionLog::resume(
                    &path,
                    &self.session.provider_id,
                    self.session.model.as_deref(),
                    self.session.model_label.as_deref(),
                    self.session.effort.as_deref(),
                    self.session.items.len(),
                );
                self.push_note(&format!("forked into {}", path.display()));
            }
            Err(error) => self.push_line(StyledLine::styled(
                error.to_string(),
                LineStyle { error: true, ..Default::default() },
            )),
        }
    }

    fn cmd_usage(&mut self) {
        let totals = App::usage_totals(&self.session.items);
        let turns = self.session.user_turn_count();
        let worked = turnip_core::text::format_elapsed(self.session.worked_ms);
        let mut lines = vec![
            format!("user turns: {turns}"),
            format!(
                "tokens in: {}  out: {}",
                totals.input_tokens.unwrap_or(0),
                totals.output_tokens.unwrap_or(0)
            ),
        ];
        if let Some(cost) = totals.cost {
            lines.push(format!("cost: ${cost:.4} (provider-reported)"));
        }
        lines.push(format!("agent time: {worked}"));
        self.push_note("session:");
        for line in lines {
            self.push_line(StyledLine::styled(
                format!("  {line}"),
                LineStyle { dim: true, ..Default::default() },
            ));
        }
    }

    fn cmd_session(&mut self) {
        match &self.session_log {
            Some(log) if log.materialized() => self.push_note(&format!(
                "session: {}  (resume with `turnip --resume {}`)",
                log.path().display(),
                log.path().display()
            )),
            Some(_log) => self.push_note("session: will be recorded when the first turn completes"),
            None => self.push_note("session recording is disabled"),
        }
    }

    fn cmd_config(&mut self, rest: &str) {
        let mut parts = rest.split_whitespace();
        let key = parts.next();
        let value = parts.next();
        match (key, value) {
            (None, _) => {
                self.push_note(&format!(
                    "config: {}  state: {}",
                    self.config.file_path().display(),
                    self.config.state_path().display()
                ));
                for setting in turnip_core::config::SETTINGS {
                    let resolved = self
                        .config
                        .resolve(setting.key)
                        .map(|(value, source)| format!("{value}  ({})", source.as_str()))
                        .unwrap_or_else(|| "unset".into());
                    self.push_line(StyledLine::styled(
                        format!("  {:<24} {}", setting.key, resolved),
                        LineStyle { dim: true, ..Default::default() },
                    ));
                }
            }
            (Some(key), None) => {
                match self.config.resolve(key) {
                    Some((value, source)) => self.push_note(&format!("{key} = {value} ({})", source.as_str())),
                    None => self.push_note(&format!("{key} is unset")),
                }
            }
            (Some(key), Some(value)) => match self.config.persist(key, Some(value)) {
                Ok(()) => {
                    self.config.set_run(key, None);
                    self.push_note(&format!("{key} = {value} (saved)"));
                }
                Err(error) => self.push_line(StyledLine::styled(
                    error.to_string(),
                    LineStyle { error: true, ..Default::default() },
                )),
            },
        }
    }
}

/// The streaming renderer, wired to the agent loop for one turn.
struct TurnRenderer<'a> {
    terminal: &'a mut Terminal,
    editor: &'a mut LineEditor,
    state: &'a mut RenderState,
    signal: &'a AbortSignal,
    queued_prompt: &'a mut Option<String>,
    markdown: bool,
    show_reasoning: bool,
}

impl<'a> TurnRenderer<'a> {
    /// Flush every complete markdown line of the assistant stream.
    fn flush_stream(&mut self) {
        let (tail_start, _) = markdown::split_for_flush(&self.state.stream_text);
        if tail_start > self.state.flushed_chars {
            let mut chunk =
                self.state.stream_text[self.state.flushed_chars..tail_start].to_string();
            // A blank line the chunk ends on is held in the tail: the next
            // chunk renders it as the break before it, and `finish` drops
            // it when the stream ends there. Committing it now would
            // leave a stray blank line at every flush boundary.
            let committed = chunk.len() - trailing_break_len(&chunk);
            chunk.truncate(committed);
            if !chunk.is_empty() {
                let lines = self.text_lines(&chunk);
                self.commit_block(Channel::Assistant, lines);
            }
            self.state.flushed_chars += committed;
        }
        // Reasoning lines flush the same way, as a dimmed italic aside.
        if self.state.reasoning_text.len() > self.state.reasoning_flushed {
            let complete = self.state.reasoning_text.rsplit('\n').count();
            if self.state.reasoning_text.ends_with('\n') || complete > 1 {
                let safe = self
                    .state
                    .reasoning_text
                    .char_indices()
                    .rev()
                    .find(|(_index, ch)| *ch == '\n')
                    .map(|(index, _)| index + 1)
                    .unwrap_or(self.state.reasoning_text.len());
                if safe > self.state.reasoning_flushed {
                    let chunk =
                        self.state.reasoning_text[self.state.reasoning_flushed..safe].to_string();
                    let aside = LineStyle {
                        dim: true,
                        italic: true,
                        ..Default::default()
                    };
                    let lines: Vec<StyledLine> = chunk
                        .split('\n')
                        .filter(|line| !line.is_empty())
                        .map(|line| StyledLine::styled(line.to_string(), aside))
                        .collect();
                    self.commit_block(Channel::Reasoning, lines);
                    self.state.reasoning_flushed = safe;
                }
            }
        }
    }

    /// Render a chunk of assistant text into lines for the scrollback.
    fn text_lines(&self, chunk: &str) -> Vec<StyledLine> {
        if self.markdown {
            markdown::render(chunk, self.terminal_width())
        } else {
            chunk.lines().map(StyledLine::plain).collect()
        }
    }

    /// Commit one block of stream output to the scrollback, keeping the
    /// separation between message kinds exact: blocks of a different
    /// channel open with one blank line, a paragraph break split across
    /// chunks lands as one line, never two, and a block never doubles one.
    fn commit_block(&mut self, channel: Channel, mut lines: Vec<StyledLine>) {
        let changed_channel = self.state.last_channel.is_some_and(|last| last != channel);
        if self.state.last_blank || changed_channel {
            // The break this block opens with is already on screen, or the
            // separator below supplies it; do not write it twice.
            let leading = lines.iter().take_while(|l| l.text.is_empty()).count();
            lines.drain(..leading);
        }
        if lines.is_empty() {
            return;
        }
        if changed_channel && !self.state.last_blank {
            lines.insert(0, StyledLine::plain(""));
        }
        self.state.last_channel = Some(channel);
        self.state.last_blank = lines.last().is_some_and(|l| l.text.is_empty());
        self.push_scrollback(lines);
    }

    /// Commit a finished tool block: the call line, then a preview of its
    /// result — the tail of what streamed live (process output, diff
    /// previews), or the head of the model-facing result when nothing
    /// streamed (file reads, errors).
    ///
    /// Results go in hax's tool box: gutter glyphs carry the rows — ┌
    /// opens, │ runs, └ closes (a lone row closes; hax's › marker is
    /// turnip's prompt glyph) — in plain dim, so the rows read as the
    /// call's output and never as a second accent line. The call header
    /// takes bold accent, which no result row shares, so the two never
    /// blur even where the terminal draws faint only subtly.
    fn commit_tool_view(&mut self, tool: &ToolView, summary: &str) {
        let width = self.terminal_width();
        let mut lines = vec![StyledLine::styled(
            format!("▸ {}({})", tool.name, tool.argument),
            LineStyle {
                accent: true,
                bold: true,
                ..Default::default()
            },
        )];
        let shown: Vec<&str> = if tool.output.trim().is_empty() {
            summary.lines().take(3).collect()
        } else {
            let mut tail: Vec<&str> = tool.output.lines().rev().take(15).collect();
            tail.reverse();
            tail
        };
        let dim = LineStyle {
            dim: true,
            ..Default::default()
        };
        let total = shown.len();
        for (index, line) in shown.into_iter().enumerate() {
            let gutter = if total == 1 {
                "└"
            } else if index == 0 {
                "┌"
            } else if index + 1 == total {
                "└"
            } else {
                "│"
            };
            lines.push(StyledLine::styled(
                format!(
                    "{gutter} {}",
                    turnip_core::text::truncate_columns(line, width.saturating_sub(4))
                ),
                dim,
            ));
        }
        self.commit_block(Channel::Tool, lines);
    }

    fn terminal_width(&self) -> usize {
        self.terminal.size().0
    }

    fn push_scrollback(&mut self, lines: Vec<StyledLine>) {
        if lines.is_empty() {
            return;
        }
        let _ = self.terminal.push_lines(&lines);
    }

    /// Compose and draw the live region for the current state.
    fn render(&mut self) {
        self.flush_stream();
        let width = self.terminal_width();
        let (_, height) = self.terminal.size();
        let mut lines = Vec::new();

        // Assistant tail (unflushed stream text).
        if self.state.flushed_chars < self.state.stream_text.len() {
            let tail = &self.state.stream_text[self.state.flushed_chars..];
            if self.markdown {
                lines.extend(markdown::render(tail, width));
            } else {
                for line in tail.split('\n') {
                    lines.push(StyledLine::plain(line));
                }
            }
        }
        // Open reasoning buffer.
        if self.show_reasoning {
            let pending = &self.state.reasoning_text
                [self.state.reasoning_flushed.min(self.state.reasoning_text.len())..];
            if !pending.is_empty() {
                lines.push(StyledLine::styled(
                    pending.lines().last().unwrap_or("").to_string(),
                    LineStyle {
                        dim: true,
                        italic: true,
                        ..Default::default()
                    },
                ));
            }
        }
        // Running tool view: the call header, then live output rows with
        // hax's gutter — │ for settled rows, ╵ for the newest, still-open
        // one. Committing swaps them into the closed ┌/│/└ box.
        if let Some(tool) = &self.state.tool {
            lines.push(StyledLine::styled(
                format!("▸ {}({})", tool.name, tool.argument),
                LineStyle {
                    accent: true,
                    bold: true,
                    ..Default::default()
                },
            ));
            let mut tail: Vec<&str> = tool.output.lines().rev().take(6).collect();
            tail.reverse();
            let total = tail.len();
            for (index, line) in tail.into_iter().enumerate() {
                let gutter = if index + 1 == total { "╵" } else { "│" };
                lines.push(StyledLine::styled(
                    format!(
                        "{gutter} {}",
                        turnip_core::text::truncate_columns(line, width.saturating_sub(4))
                    ),
                    LineStyle {
                        dim: true,
                        ..Default::default()
                    },
                ));
            }
        }
        // Status line.
        lines.push(self.status_line());
        // Input line(s).
        lines.extend(self.editor.view(width, height.saturating_sub(2)).0);
        if let Some(prompt) = &self.queued_prompt {
            lines.push(StyledLine::styled(
                format!("queued: {prompt}"),
                LineStyle { dim: true, ..Default::default() },
            ));
        }

        // The terminal clips overflow after wrapping, in physical rows.
        let _ = self.terminal.render_region(&lines);
    }

    fn status_line(&mut self) -> StyledLine {
        let elapsed = turnip_core::text::format_elapsed(
            self.state.turn_started.elapsed().as_millis() as i64,
        );
        self.state.spinner = (self.state.spinner + 1) % SPINNER.len();
        let frame = SPINNER[self.state.spinner];
        let mut text = format!("{frame} {elapsed}");
        if let Some(note) = &self.state.status_note {
            text.push_str(&format!(" · {note}"));
        }
        if let Some(usage) = &self.state.turn_usage {
            let tokens = format_tokens(usage);
            if !tokens.is_empty() {
                text.push_str(&format!(" · {tokens}"));
            }
        }
        StyledLine::styled(text, LineStyle { dim: true, ..Default::default() })
    }

    /// Called by the turn loop after every stream; keep the interrupt flag
    /// and queued prompts in sync.
    fn pump(&mut self) -> bool {
        if self.signal.is_aborted() {
            return false;
        }
        // Type-ahead: keys typed while streaming land in the editor.
        let events = match self.terminal.pump() {
            Ok(events) => events,
            Err(_) => return false,
        };
        for event in events {
            match event {
                crate::term::TermEvent::Key(key) => {
                    if crate::term::is_interrupt(&key) {
                        self.state.interrupted = true;
                        self.signal.abort();
                        return false;
                    }
                    if key.code == crossterm::event::KeyCode::Enter
                        && !key.modifiers.contains(crossterm::event::KeyModifiers::ALT)
                    {
                        let prompt = self.editor.take();
                        if !prompt.trim().is_empty() {
                            *self.queued_prompt = Some(prompt);
                        }
                        continue;
                    }
                    let _ = self.editor.handle_key(&key, self.terminal);
                }
                crate::term::TermEvent::Paste(text) => self.editor.insert_str(&text),
                crate::term::TermEvent::Resize => {}
            }
        }
        true
    }

    /// Flush everything and reset the live region; returns the outcome.
    fn finish(&mut self, outcome: TurnOutcome) -> TurnOutcome {
        self.flush_stream();
        // Anything still unflushed goes out now; a break the stream ended on
        // is dropped so the transcript never ends in a blank line.
        if self.state.flushed_chars < self.state.stream_text.len() {
            let remainder = self.state.stream_text[self.state.flushed_chars..].to_string();
            let mut lines = self.text_lines(&remainder);
            while lines.last().is_some_and(|l| l.text.is_empty()) {
                lines.pop();
            }
            self.commit_block(Channel::Assistant, lines);
            self.state.flushed_chars = self.state.stream_text.len();
        }
        if self.show_reasoning && self.state.reasoning_flushed < self.state.reasoning_text.len() {
            let remainder = self.state.reasoning_text[self.state.reasoning_flushed..].to_string();
            let aside = LineStyle {
                dim: true,
                italic: true,
                ..Default::default()
            };
            let lines: Vec<StyledLine> = remainder
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| StyledLine::styled(line.to_string(), aside))
                .collect();
            self.commit_block(Channel::Reasoning, lines);
            self.state.reasoning_flushed = self.state.reasoning_text.len();
        }
        // A tool still open (interrupted mid-run) closes its block too.
        if let Some(tool) = self.state.tool.take() {
            self.commit_tool_view(&tool, "");
        }
        let _ = self.terminal.render_region(&[StyledLine::plain("")]);
        outcome
    }
}

impl<'a> TurnHooks for TurnRenderer<'a> {
    fn on_event(&mut self, event: &StreamEvent) -> bool {
        match event {
            StreamEvent::TextDelta { text } => {
                self.state.stream_text.push_str(text);
            }
            StreamEvent::ReasoningDelta { text, .. } => {
                if self.show_reasoning {
                    self.state
                        .reasoning_text
                        .push_str(text.as_deref().unwrap_or("…"));
                    self.state.reasoning_text.push('\n');
                }
            }
            StreamEvent::Retry {
                attempt,
                max_attempts,
                delay_ms,
                http_status,
            } => {
                self.state.status_note = Some(format!(
                    "retrying {attempt}/{max_attempts} in {delay_ms} ms (HTTP {http_status})"
                ));
            }
            StreamEvent::Done { usage, .. } => {
                self.state.turn_usage = Some(*usage);
                self.state.status_note = None;
            }
            StreamEvent::Error { .. } | StreamEvent::ToolCallStart { .. } => {}
            StreamEvent::ToolCallDelta { .. } | StreamEvent::ToolCallEnd { .. } => {}
            StreamEvent::ReasoningItem { .. } | StreamEvent::Progress { .. } => {}
        }
        self.render();
        self.pump()
    }

    fn on_tool_start(&mut self, call: &agent::PendingCallView) -> bool {
        // Flush the assistant text so the tool line sits below it.
        self.flush_stream();
        self.state.tool = Some(ToolView {
            name: call.name.clone(),
            argument: tool_argument_display(&call.name, &call.arguments),
            output: String::new(),
        });
        self.render();
        self.pump()
    }

    fn on_tool_output(&mut self, bytes: &[u8]) {
        if let Some(tool) = &mut self.state.tool {
            tool.output.push_str(&String::from_utf8_lossy(bytes));
            let cap = 8000;
            if tool.output.len() > cap * 2 {
                let start = tool.output.len() - cap;
                tool.output = tool.output[start..].to_string();
            }
        }
    }

    fn on_tool_note(&mut self, text: &str) {
        if let Some(tool) = &mut self.state.tool {
            tool.output.push_str(text);
        }
    }

    fn on_tool_end(&mut self, call: &agent::PendingCallView, output: &str) {
        if let Some(tool) = self.state.tool.take() {
            self.commit_tool_view(&tool, output);
        }
        let _ = call;
    }

    fn tick(&mut self) -> bool {
        if !self.pump() {
            return false;
        }
        // Animate at a human pace, not per poll.
        let now = Instant::now();
        let due = self
            .state
            .last_render
            .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(90));
        if due {
            self.state.last_render = Some(now);
            self.render();
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
        !self.signal.is_aborted()
    }

    fn on_compaction(&mut self, started: bool) {
        let note = if started {
            "── compacting conversation ──"
        } else {
            "── compacted ──"
        };
        self.push_scrollback(vec![StyledLine::styled(
            note,
            LineStyle { dim: true, ..Default::default() },
        )]);
    }
}

/// Length of the trailing blank lines at the end of a flushed chunk. Each
/// is a bare `\n` (its line ending is the newline before it), so a chunk
/// ending on a paragraph break holds exactly its trailing newline run.
fn trailing_break_len(chunk: &str) -> usize {
    let bytes = chunk.as_bytes();
    let mut len = 0;
    while len < bytes.len() {
        let at = bytes.len() - len - 1;
        if bytes[at] == b'\n' && (at == 0 || bytes[at - 1] == b'\n') {
            len += 1;
        } else {
            break;
        }
    }
    len
}

/// A short single-argument display for the tool call line.
fn tool_argument_display(name: &str, arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return turnip_core::text::truncate_columns(arguments, 48);
    };
    let candidate = match name {
        "bash" => value.get("command").and_then(serde_json::Value::as_str),
        "read" | "write" | "edit" => value.get("path").and_then(serde_json::Value::as_str),
        _ => None,
    };
    match candidate {
        Some(text) => turnip_core::text::truncate_columns(text, 48),
        None => turnip_core::text::truncate_columns(arguments, 48),
    }
}

/// Compact "in 1.2k · out 340" style token totals.
fn format_tokens(usage: &StreamUsage) -> String {
    let mut parts = Vec::new();
    if let Some(input) = usage.input_tokens {
        parts.push(format!("in {}", compact_count(input)));
    }
    if let Some(output) = usage.output_tokens {
        parts.push(format!("out {}", compact_count(output)));
    }
    if let Some(cost) = usage.cost {
        parts.push(format!("${cost:.4}"));
    }
    parts.join(" · ")
}

fn compact_count(value: i64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

/// Expand `@path` mentions that exist on disk into absolute paths.
fn expand_mentions(prompt: &str, cwd: &Path) -> String {
    let mut expanded = String::new();
    let mut rest = prompt;
    while let Some(at) = rest.find('@') {
        let (before, after) = rest.split_at(at);
        expanded.push_str(before);
        let mention: String = after[1..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
            .collect();
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from);
        let resolved = if mention == "~" {
            home
        } else if let Some(home) = home.clone() {
            if let Some(rest) = mention.strip_prefix("~/") {
                Some(home.join(rest))
            } else {
                Some(cwd.join(&mention))
            }
        } else {
            Some(cwd.join(&mention))
        };
        if !mention.is_empty() && resolved.as_ref().is_some_and(|p| p.exists()) {
            expanded.push_str(
                &resolved
                    .expect("checked")
                    .to_string_lossy()
                    .into_owned(),
            );
        } else {
            expanded.push('@');
            expanded.push_str(&mention);
        }
        rest = &after[1 + mention.len()..];
    }
    expanded.push_str(rest);
    expanded
}

/// The startup banner.
fn print_banner(terminal: &mut Terminal, app: &App) {
    let provider = app
        .provider_descriptor
        .as_ref()
        .map(|d| d.display_name.clone())
        .unwrap_or_else(|| "no provider".into());
    let model = app.config.str("model").unwrap_or_default();
    let dim = LineStyle {
        dim: true,
        ..Default::default()
    };
    let mut lines = vec![
        StyledLine::styled(
            format!("turnip {} · {provider} · {model}", turnip_core::version()),
            LineStyle {
                bold: true,
                accent: true,
                ..Default::default()
            },
        ),
        StyledLine::styled(format!("cwd: {}", app.cwd.display()), dim),
    ];
    if let Some(meta) = &app.meta {
        lines.push(StyledLine::styled(
            format!("resumed session {}", meta.id),
            dim,
        ));
    }
    lines.push(StyledLine::styled(
        "/help for commands · Ctrl+C to interrupt",
        dim,
    ));
    let _ = terminal.push_lines(&lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_breaks_hold_only_bare_blank_lines() {
        assert_eq!(trailing_break_len("para\n"), 0);
        // A chunk ending on a paragraph break holds exactly its blank line.
        assert_eq!(trailing_break_len("para\n\n"), 1);
        // A chunk of nothing but breaks holds all of it.
        assert_eq!(trailing_break_len("\n"), 1);
        assert_eq!(trailing_break_len("\n\n\n"), 3);
    }

    #[test]
    fn mention_expansion_resolves_existing_paths() {
        let dir = std::env::temp_dir().join(format!("turnip-mention-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "x").unwrap();
        let expanded = expand_mentions("look at @src/lib.rs and @missing.txt", &dir);
        assert!(expanded.contains(&dir.join("src/lib.rs").to_string_lossy().to_string()));
        assert!(expanded.contains("@missing.txt"));
    }

    #[test]
    fn token_formatting() {
        let usage = StreamUsage {
            input_tokens: Some(1234),
            output_tokens: Some(56),
            cost: Some(0.01234),
            ..Default::default()
        };
        assert_eq!(format_tokens(&usage), "in 1.2k · out 56 · $0.0123");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_500_000), "1.5m");
    }

    #[test]
    fn tool_argument_display_extracts_primary_argument() {
        assert_eq!(
            tool_argument_display("bash", r#"{"command": "ls -la"}"#),
            "ls -la"
        );
        assert_eq!(
            tool_argument_display("edit", r#"{"path": "a/b.txt"}"#),
            "a/b.txt"
        );
    }
}
