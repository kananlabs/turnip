//! turnip-core: the library behind the turnip coding agent.
//!
//! A provider-independent conversation model, streaming provider adapters
//! (OpenAI-compatible Chat Completions and Anthropic-compatible Messages),
//! an agent loop with tool execution and compaction, terminal-agnostic tools,
//! tiered configuration, and append-only session persistence — with no
//! global state, so the whole crate embeds cleanly in other applications
//! (e.g. Python bindings via PyO3/maturin).
//!
//! ```no_run
//! use turnip_core::{config::Config, provider, tools::{ToolEnv, ToolRegistry}, agent::{self, AgentSession, TurnHooks}};
//!
//! let config = Config::load();
//! let transport = provider::registry::transport_from_config(&config);
//! let (descriptor, provider) = provider::registry::autoselect(&config, &transport)
//!     .expect("no provider available");
//! let mut session = AgentSession::new(provider.as_ref(), &config, false);
//! session.add_user("explain this repository");
//! let tools = ToolRegistry::new(ToolEnv {
//!     cwd: std::env::current_dir().unwrap(),
//!     config: config.clone(),
//! });
//! let mut hooks = ();
//! let outcome = agent::run_user_turn(
//!     &mut session, provider.as_ref(), &tools, &config, &mut hooks,
//!     &provider::AbortSignal::new(),
//! );
//! ```

pub mod agent;
pub mod config;
pub mod effort;
pub mod error;
pub mod item;
pub mod model;
pub mod provider;
pub mod session;
pub mod text;
pub mod tools;
pub mod transport;

pub use error::{CoreError, Result};
pub use item::Item;
pub use agent::AgentSession;
pub use config::Config;

/// The crate version, for banners and session headers.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Empty implementation of [`agent::TurnHooks`] for one-shot callers.
impl agent::TurnHooks for () {}
