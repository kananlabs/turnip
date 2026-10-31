//! Environment assembly: the system prompt built from the base instructions,
//! the working context, AGENTS.md, and user overrides.

use std::path::Path;

use crate::config::Config;
use crate::text;

const BASE_PROMPT: &str = "You are turnip, a pragmatic terminal coding agent. Help the user with \
software engineering: reading and editing code, running commands, and answering questions about \
their project.

Rules:
- Ground every claim in the repository: read a file before describing it, run the command before \
claiming its output.
- Prefer the read, edit, write, and bash tools for anything that touches files or processes; \
answer from general knowledge only when the user asks a question unrelated to the work at hand.
- Keep edits minimal and matched to the request; do not reformat or \"improve\" code the user \
did not ask about.
- After running a command, report what happened plainly, including failures.
- Prefer short, direct answers. Use Markdown for structure when it helps, not for decoration.
- Ask before destructive operations you cannot undo (deleting files outside the repo, force-push, \
schema drops).";
const AGENTS_FILE: &str = "AGENTS.md";
const AGENTS_LIMIT: u64 = 64 * 1024;

/// Build the effective system prompt for the current working directory.
pub fn build_system_prompt(config: &Config) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut prompt = match config.str("system_prompt") {
        Some(value) => match expand_prompt_file(config, &value) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("warning: {error}");
                BASE_PROMPT.to_string()
            }
        },
        None => BASE_PROMPT.to_string(),
    };

    // Working context.
    prompt.push_str(&format!(
        "\n\nEnvironment:\n- Working directory: {}\n- Platform: {}\n- Date: {}",
        cwd.display(),
        std::env::consts::OS,
        crate::session::format_timestamp_now(),
    ));
    if let Some((branch, subject)) = git_state(&cwd) {
        prompt.push_str(&format!("\n- Git branch: {branch} (HEAD subject: {subject})"));
    }

    // Project instructions.
    if !config.bool_or("no_agents_md", false) {
        if let Ok(bytes) = std::fs::read(cwd.join(AGENTS_FILE)) {
            if bytes.len() as u64 <= AGENTS_LIMIT {
                let contents = text::sanitize_utf8(&bytes);
                let contents = contents.trim();
                if !contents.is_empty() {
                    prompt.push_str(&format!(
                        "\n\nProject instructions from {AGENTS_FILE}:\n{contents}"
                    ));
                }
            }
        }
    }

    if let Some(appendix) = config.str("system_prompt_append") {
        match expand_prompt_file(config, &appendix) {
            Ok(text) if !text.is_empty() => prompt.push_str(&format!("\n\n{text}")),
            Ok(_) => {}
            Err(error) => eprintln!("warning: {error}"),
        }
    }
    prompt
}

/// A leading '@' names a file to read the prompt from ('~' expands to the
/// home directory; relative paths resolve against the config directory).
/// Other values pass through verbatim.
fn expand_prompt_file(config: &Config, value: &str) -> Result<String, String> {
    let Some(path) = value.strip_prefix('@') else {
        return Ok(value.to_string());
    };
    let mut path = path.to_string();
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            path = home.join(&path[2..]).to_string_lossy().into_owned();
        }
    }
    let resolved = Path::new(&path);
    let resolved = if resolved.is_absolute() {
        resolved.to_path_buf()
    } else {
        config
            .file_path()
            .parent()
            .unwrap_or(Path::new("."))
            .join(resolved)
    };
    let bytes = std::fs::read(&resolved)
        .map_err(|e| format!("cannot read system prompt file {}: {e}", resolved.display()))?;
    if bytes.len() as u64 > 256 * 1024 {
        return Err(format!(
            "system prompt file {} is too large",
            resolved.display()
        ));
    }
    Ok(text::sanitize_utf8(&bytes).trim().to_string())
}

/// Current git branch and HEAD subject, when the cwd is a repository.
fn git_state(cwd: &Path) -> Option<(String, String)> {
    let run = |args: &[&str]| -> Option<String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let subject = run(&["log", "-1", "--pretty=%s"]).unwrap_or_default();
    if branch.is_empty() && subject.is_empty() {
        return None;
    }
    Some((branch, subject))
}
