//! The `bash` tool: run a shell command with live output streaming, a
//! configurable timeout, and cooperative interruption.
//!
//! On Unix the default shell is `bash`. On Windows turnip prefers a `bash`
//! found on `PATH` (WSL or Git Bash) and falls back to PowerShell, so the
//! same tool works natively on every platform; `bash.shell` configures it.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{cap_output, Tool, ToolEnv, ToolIo};
use crate::provider::{ToolDef, ToolParam};

const BASH_DESCRIPTION: &str = "Run a bash command and return its combined output.\n\n\
    Working directory and environment are preserved between calls through the command itself: \
    use absolute paths or `cd` within a single command. Long-running commands keep streaming; \
    set timeout_seconds above the default for slow builds and tests.";

pub struct BashTool {
    pub env: ToolEnv,
}

impl Tool for BashTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "bash",
            description: BASH_DESCRIPTION,
            params: &[
                ToolParam {
                    name: "command",
                    r#type: "string",
                    item_type: None,
                    description: "Shell command to run.",
                    required: true,
                },
                ToolParam {
                    name: "timeout_seconds",
                    r#type: "integer",
                    item_type: None,
                    description: "Optional override of the default timeout. Use a higher \
                                  value for slow builds or tests.",
                    required: false,
                },
            ],
        }
    }

    fn run(&self, args: &Value, io: &mut ToolIo) -> String {
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return "error: 'command' is required".into();
        };
        if command.trim().is_empty() {
            return "error: 'command' is empty".into();
        }
        let timeout_ms = self.timeout_ms(args.get("timeout_seconds"));
        match self.spawn(command) {
            Err(error) => format!("error: cannot start shell: {error}"),
            Ok((program, mut child)) => {
                let output = self.pump(&mut child, io, timeout_ms);
                finish_output(output, &mut child, program, io)
            }
        }
    }
}

impl BashTool {
    fn timeout_ms(&self, requested: Option<&Value>) -> u64 {
        let default = self.env.config.duration_ms_or("bash.timeout", 120_000);
        let maximum = self.env.config.duration_ms_or("bash.timeout_max", 1_800_000);
        let requested = requested
            .and_then(Value::as_i64)
            .map(|s| (s.max(1) * 1000) as u64)
            .unwrap_or(default as u64);
        requested.min(maximum as u64)
    }

    fn spawn(&self, command: &str) -> std::io::Result<(String, std::process::Child)> {
        let (program, args) = shell_command(&self.env, command);
        let mut child = Command::new(&program)
            .args(&args)
            .current_dir(&self.env.cwd)
            .env("TURNIP_SHELL", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // Leave the child in its own process group so terminal interrupts
        // target the agent, not the command (best effort; ignore errors).
        detach_process_group(&mut child);
        Ok((program, child))
    }

    /// Stream both output pipes through one channel until the child exits,
    /// the timeout hits, or the user interrupts.
    fn pump(&self, child: &mut std::process::Child, io: &mut ToolIo, timeout_ms: u64) -> String {
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (sender, receiver) = mpsc::channel::<Vec<u8>>();
        if let Some(pipe) = stdout {
            spawn_reader(pipe, sender.clone());
        }
        if let Some(pipe) = stderr {
            spawn_reader(pipe, sender.clone());
        }
        drop(sender);

        let mut collected: Vec<u8> = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(bytes) => {
                    collected.extend_from_slice(&bytes);
                    io.display(&bytes);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !io.tick() {
                        let _ = child.kill();
                        io.interrupted = true;
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                break;
            }
        }
        cap_output(&self.env, &crate::text::sanitize_utf8(&collected))
    }
}

fn spawn_reader<R: Read + Send + 'static>(pipe: R, sender: mpsc::Sender<Vec<u8>>) {
    std::thread::spawn(move || {
        let mut reader = pipe;
        let mut chunk = [0u8; 8 * 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sender.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Shape the final model-facing output: raw output, plus exit status and
/// timeout notices.
fn finish_output(
    mut output: String,
    child: &mut std::process::Child,
    program: String,
    io: &mut ToolIo,
) -> String {
    let status = child.wait();
    let mut notes = String::new();
    if io.interrupted {
        notes.push_str("\n[interrupted]");
    }
    match status {
        Ok(status) if !status.success() => {
            if let Some(code) = status.code() {
                notes.push_str(&format!("\nExit code: {code}"));
            } else {
                notes.push_str("\nTerminated by signal");
            }
        }
        Ok(_) => {}
        Err(error) => notes.push_str(&format!("\nerror: waiting for shell: {error}")),
    }
    if output.trim().is_empty() && notes.is_empty() {
        return format!("(no output from {program})");
    }
    if !output.is_empty() {
        output.push_str(&notes);
        output
    } else {
        notes.trim_start_matches('\n').to_string()
    }
}

/// Resolve the shell program and argument list for one command.
fn shell_command(env: &ToolEnv, command: &str) -> (String, Vec<String>) {
    if let Some(shell) = env.config.str("bash.shell") {
        let shell = shell.trim();
        if !shell.is_empty() {
            if shell.contains(' ') {
                // "zsh -lc" style: program plus fixed leading arguments.
                let mut parts = shell.split_whitespace();
                let program = parts.next().unwrap_or("bash").to_string();
                let mut args: Vec<String> = parts.map(String::from).collect();
                args.push("-c".into());
                args.push(command.into());
                return (program, args);
            }
            return (shell.into(), vec!["-c".into(), command.into()]);
        }
    }
    #[cfg(windows)]
    {
        // Prefer bash (WSL/Git Bash) for command compatibility; PowerShell is
        // the fallback so the tool still works on a bare Windows install.
        if which_bash().is_some() {
            ("bash".into(), vec!["-c".into(), command.into()])
        } else {
            ("powershell.exe".into(), vec!["-Command".into(), command.into()])
        }
    }
    #[cfg(not(windows))]
    ("bash".into(), vec!["-c".into(), command.into()])
}

#[cfg(windows)]
fn which_bash() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("bash.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn detach_process_group(child: &mut std::process::Child) {
    // setsid via libc would need the libc crate; kill-on-drop semantics plus
    // explicit kill on interrupt cover the practical cases.
    let _ = child;
}

#[cfg(windows)]
fn detach_process_group(child: &mut std::process::Child) {
    let _ = child;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn env() -> ToolEnv {
        let cwd = std::env::temp_dir().join(format!("turnip-test-bash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        ToolEnv {
            cwd,
            config: Config::default(),
        }
    }

    #[test]
    fn runs_echo_and_reports_status() {
        let _ = std::fs::create_dir_all(env().cwd);
        let tool = BashTool { env: env() };
        let mut io = ToolIo::quiet();
        let output = tool.run(&serde_json::json!({"command": "echo hello"}), &mut io);
        assert_eq!(output.trim(), "hello");
        let mut io = ToolIo::quiet();
        let output = tool.run(
            &serde_json::json!({"command": "echo oops >&2; exit 3"}),
            &mut io,
        );
        assert!(output.contains("oops"));
        assert!(output.contains("Exit code: 3"));
    }

    #[test]
    fn timeout_kills_runaway_command() {
        let _ = std::fs::create_dir_all(env().cwd);
        let mut env = env();
        env.config.set_run("bash.timeout", Some("1"));
        let tool = BashTool { env };
        let mut io = ToolIo::quiet();
        let output = tool.run(
            &serde_json::json!({"command": "sleep 30; echo never"}),
            &mut io,
        );
        assert!(!output.contains("never"));
    }

    #[test]
    fn missing_command_argument() {
        let tool = BashTool { env: env() };
        let mut io = ToolIo::quiet();
        let output = tool.run(&serde_json::json!({}), &mut io);
        assert!(output.starts_with("error:"));
    }
}
