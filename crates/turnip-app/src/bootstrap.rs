//! Startup shared by the interactive and one-shot frontends: configuration
//! loading, provider and model resolution, session resume/fork, and the
//! session log.

use std::path::PathBuf;

use turnip_core::config::Config;
use turnip_core::item::Item;
use turnip_core::provider::{self, Provider, ProviderDescriptor};
use turnip_core::session::{self, SessionLoaded, SessionLog, SessionMeta};
use turnip_core::AgentSession;

/// Everything the frontends need, assembled by [`App::bootstrap`].
pub struct App {
    pub config: Config,
    pub cwd: PathBuf,
    /// One-shot JSONL output requested.
    pub json: bool,
    /// The live provider; None until resolved (pickers may choose one).
    pub provider: Option<Box<dyn Provider>>,
    pub provider_descriptor: Option<ProviderDescriptor>,
    /// A session loaded from disk (resume/fork/continue), adopted at turn time.
    pub loaded: Option<SessionLoaded>,
    pub meta: Option<SessionMeta>,
    pub session_log: Option<SessionLog>,
    /// Raw mode: no tools, only user content.
    pub raw: bool,
    /// The model was discovered from live local-server state and must not be
    /// persisted.
    pub model_discovered: bool,
}

/// Command-line selection flags, applied as config run overrides.
#[derive(Debug, Default)]
pub struct Selection {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl App {
    /// Load configuration, apply run overrides, and resolve as much as
    /// possible without a terminal. Provider/model resolution happens here
    /// when configured; interactive pickers fill the gaps later.
    pub fn bootstrap(selection: &Selection, json: bool, raw: bool, no_session: bool) -> App {
        let mut config = Config::load();
        for warning in &config.warnings {
            eprintln!("warning: {warning}");
        }
        config.warnings.clear();

        // Run overrides from the command line.
        if let Some(value) = &selection.provider {
            config.set_run("provider", Some(value));
        }
        if let Some(value) = &selection.model {
            config.set_run("model", Some(value));
        }
        if let Some(value) = &selection.effort {
            config.set_run("effort", Some(value));
        }
        if no_session {
            config.set_run("no_session", Some("on"));
        }
        if raw {
            config.set_run("effort", None);
        }

        let cwd = std::env::current_dir().unwrap_or_default();
        session::prune_expired(&cwd, config.int_or("session_retention_days", 30));

        let mut app = App {
            config,
            cwd,
            json,
            provider: None,
            provider_descriptor: None,
            loaded: None,
            meta: None,
            session_log: None,
            raw,
            model_discovered: false,
        };

        app.select_provider();
        app.select_model();
        // Remember the selection for the next run, unless the model came
        // from transient server state (it must not be persisted).
        if let Some(descriptor) = app.provider_descriptor.clone() {
            let model = app.config.str("model");
            if !app.model_discovered {
                let _ = app.config.persist_selection(
                    &descriptor.id,
                    model.as_deref(),
                    app.config.str("effort").as_deref(),
                );
            }
        }
        app
    }

    /// Resolve the provider from explicit selection or autoselection.
    /// Leaves it unset when nothing is available; interactive mode then
    /// opens a picker.
    pub fn select_provider(&mut self) {
        let transport = provider::registry::transport_from_config(&self.config);
        if let Some(name) = self.config.str("provider") {
            if let Some(descriptor) = provider::registry::find(&self.config, &name) {
                if let Err(reason) =
                    provider::registry::availability(&self.config, &descriptor, &transport)
                {
                    eprintln!("warning: {reason}");
                }
                self.provider_descriptor = Some(descriptor.clone());
                self.provider = provider::registry::build(&self.config, &descriptor, transport);
                return;
            }
            eprintln!("warning: unknown provider '{name}'");
        }
        if let Some((descriptor, built)) = provider::registry::autoselect(&self.config, &transport) {
            // Persist the selection so the next run starts here.
            self.provider_descriptor = Some(descriptor.clone());
            self.provider = Some(built);
        }
    }

    /// Resolve the model: configuration, then the provider default, then
    /// live discovery for local servers.
    fn select_model(&mut self) {
        if self.config.str("model").is_some() {
            return;
        }
        let Some(provider) = &self.provider else {
            return;
        };
        if let Some(default) = provider.default_model() {
            self.config.set_run("model", Some(&default));
            return;
        }
        if provider.is_local() {
            if let Ok(models) = provider.list_models() {
                if let Some(first) = models.first() {
                    // Discovered from transient server state; do not persist.
                    self.config.set_run("model", Some(&first.id));
                    self.model_discovered = true;
                }
            }
        }
    }

    /// Adopt the loaded session currently stored in [`App::loaded`]: restore
    /// its selection into the conversation tier (unless this run overrides
    /// it), take over its items, and reopen its file for appending.
    pub fn adopt_loaded(&mut self) {
        let Some(loaded) = self.loaded.take() else {
            return;
        };
        // A provider switch invalidates the recorded model and effort.
        let selected_provider = self
            .provider_descriptor
            .as_ref()
            .map(|d| d.id.clone())
            .or_else(|| self.config.str("provider"));
        let provider_changed = selected_provider.is_some_and(|id| {
            loaded
                .meta
                .provider
                .as_deref()
                .is_none_or(|recorded| recorded != id)
        });
        if !provider_changed {
            if self.config.str_below_run("model").is_none() {
                if let Some(model) = loaded.meta.model.clone() {
                    if !model.is_empty() {
                        self.config.set_conversation("model", Some(&model));
                    }
                }
            }
            if self.config.str_below_run("effort").is_none() {
                if let Some(effort) = loaded.meta.effort.clone() {
                    if !effort.is_empty() {
                        self.config.set_conversation("effort", Some(&effort));
                    }
                }
            }
        }
        let meta = loaded.meta.clone();
        self.meta = Some(meta.clone());
        self.session_log = SessionLog::resume(
            &loaded.path,
            meta.provider.as_deref().unwrap_or("none"),
            meta.model.as_deref(),
            meta.model_label.as_deref(),
            meta.effort.as_deref(),
            loaded.items.len(),
        );
        self.loaded = Some(loaded);
    }

    /// Load a session file from disk (without adopting it yet).
    pub fn load_session(&mut self, path: &std::path::Path) -> Result<(), String> {
        let loaded = turnip_core::session::load_all(path).map_err(|e| e.to_string())?;
        self.loaded = Some(loaded);
        Ok(())
    }

    /// Build the agent session against `provider`.
    pub fn build_session(&self, provider: &dyn Provider) -> AgentSession {
        let mut session = AgentSession::new(provider, &self.config, self.raw);
        if let Some(loaded) = &self.loaded {
            session.items = loaded.items.clone();
            session.retired = loaded.retired.clone();
            session.worked_ms = loaded.worked_ms;
            session.last_user_turn_ms = loaded.last_user_turn_ms;
            if let Some(meta) = &self.meta {
                if !meta.id.is_empty() {
                    session.session_id = meta.id.clone();
                }
            }
        }
        session
    }



    /// The newest session for the cwd, if any (used by --continue).
    pub fn newest_session(&self) -> Option<PathBuf> {
        let entries = session::list(&self.cwd);
        entries.first().map(|entry| entry.path.clone())
    }

    /// Sum of usage recorded in the session items so far.
    pub fn usage_totals(items: &[Item]) -> turnip_core::item::StreamUsage {
        let mut totals = turnip_core::item::StreamUsage::default();
        for item in items {
            if let Item::Usage(usage) = item {
                totals.input_tokens = match (totals.input_tokens, usage.usage.input_tokens) {
                    (Some(a), Some(b)) => Some(a + b),
                    (a, b) => a.or(b),
                };
                totals.output_tokens = match (totals.output_tokens, usage.usage.output_tokens) {
                    (Some(a), Some(b)) => Some(a + b),
                    (a, b) => a.or(b),
                };
                totals.cost = match (totals.cost, usage.usage.cost) {
                    (Some(a), Some(b)) => Some(a + b),
                    (a, b) => a.or(b),
                };
            }
        }
        totals
    }
}
