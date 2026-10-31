//! turnip: a minimalist, terminal-native coding agent.

mod bootstrap;
mod oneshot;
mod repl;
mod term;

use clap::Parser;
use turnip_core::session;

use bootstrap::{App, Selection};

#[derive(Debug, Parser)]
#[command(
    name = "turnip",
    version,
    about = "A minimalist, terminal-native coding agent",
    disable_help_subcommand = true
)]
struct Cli {
    /// Run one prompt non-interactively and exit
    #[arg(short, long, value_name = "PROMPT")]
    prompt: Option<String>,

    /// Provider id (openai, anthropic, openrouter, deepseek, llamacpp, ollama, ...)
    #[arg(long)]
    provider: Option<String>,

    /// Model id
    #[arg(long)]
    model: Option<String>,

    /// Reasoning effort level
    #[arg(long)]
    effort: Option<String>,

    /// Resume the newest recorded session for this directory
    #[arg(short, long)]
    r#continue: bool,

    /// Resume a specific session file
    #[arg(short, long, value_name = "PATH")]
    resume: Option<String>,

    /// Branch from a session file into a new one instead of continuing it
    #[arg(long, value_name = "PATH")]
    fork: Option<String>,

    /// List recorded sessions for this directory
    #[arg(long)]
    list_sessions: bool,

    /// One-shot: print the conversation as JSONL records on stdout
    #[arg(long, requires = "prompt")]
    json: bool,

    /// Send only user content; advertise no tools
    #[arg(long)]
    raw: bool,

    /// Fresh context: skip AGENTS.md
    #[arg(long)]
    bare: bool,

    /// Disable session recording for this run
    #[arg(long)]
    no_session: bool,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if cli.list_sessions {
        return list_sessions();
    }

    let selection = Selection {
        provider: cli.provider.clone(),
        model: cli.model.clone(),
        effort: cli.effort.clone(),
    };
    let mut app = App::bootstrap(&selection, cli.json, cli.raw, cli.no_session);
    if cli.bare {
        app.config.set_run("no_agents_md", Some("on"));
    }

    // Session selection: --resume, --fork, --continue.
    let session_path: Option<std::path::PathBuf> = if let Some(path) = &cli.resume {
        Some(std::path::PathBuf::from(path))
    } else if let Some(path) = &cli.fork {
        Some(std::path::PathBuf::from(path))
    } else if cli.r#continue {
        app.newest_session()
    } else {
        None
    };
    if let Some(path) = session_path {
        if let Err(error) = app.load_session(&path) {
            eprintln!("turnip: {error}");
            return std::process::ExitCode::from(1);
        }
        if let Some(source) = &cli.fork {
            // Branch: write the loaded items into a fresh file and continue
            // recording there.
            let loaded = app.loaded.as_ref().expect("loaded above");
            let mut meta = loaded.meta.clone();
            meta.provider = Some(app.provider_descriptor.as_ref().map(|d| d.id.clone()).unwrap_or_default());
            match session::fork_file(std::path::Path::new(source), &loaded.items, &meta) {
                Ok(forked) => {
                    let _ = app.load_session(&forked);
                }
                Err(error) => {
                    eprintln!("turnip: {error}");
                    return std::process::ExitCode::from(1);
                }
            }
        }
        app.adopt_loaded();
    }

    // Interactive or one-shot.
    let interactive = cli.prompt.is_none();
    if interactive {
        match repl::Repl::start(app) {
            Ok(repl) => std::process::ExitCode::from(repl.run() as u8),
            Err(error) => {
                eprintln!("turnip: {error}");
                std::process::ExitCode::from(1)
            }
        }
    } else {
        let prompt = match cli.prompt {
            Some(prompt) => prompt,
            None => return std::process::ExitCode::from(1),
        };
        std::process::ExitCode::from(oneshot::run(&mut app, prompt) as u8)
    }
}

/// `--list-sessions`: print this directory's sessions with their first prompt.
fn list_sessions() -> std::process::ExitCode {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut entries = session::list(&cwd);
    if entries.is_empty() {
        println!("no recorded sessions for {}", cwd.display());
        return std::process::ExitCode::SUCCESS;
    }
    for entry in &mut entries {
        session::label_read(entry, 60);
        let prompt = entry
            .label
            .prompt
            .clone()
            .unwrap_or_else(|| "(no prompt recorded)".into());
        let model = entry.label.model.clone().unwrap_or_default();
        let provider = entry.label.provider.clone().unwrap_or_default();
        println!(
            "{}  {:<14} {:<28} {}",
            session::format_mtime(entry.mtime_secs),
            provider,
            model,
            prompt
        );
    }
    let newest = entries.first().map(|e| e.path.clone()).unwrap_or_default();
    println!("resume with: turnip --resume {}", newest.display());
    std::process::ExitCode::SUCCESS
}
