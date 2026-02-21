use anyhow::{bail, Context, Result};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::pty;

/// Shell-escape a string for safe embedding in a bash command.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub struct TmuxSession {
    pub name: String,
    cleaned_up: bool,
}

impl TmuxSession {
    /// Create a new tmux session running the given command.
    pub fn new(directory: Option<&str>, command: &str) -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let name = format!("agentusage-{}-{}", std::process::id(), nanos);

        let mut cmd = Command::new("tmux");
        cmd.args([
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            "200",
            "-y",
            "50",
        ]);
        if let Some(dir) = directory {
            cmd.args(["-c", dir]);
        }
        cmd.arg(command);

        let output = cmd.output().context("Failed to create tmux session")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux new-session failed: {}", stderr.trim());
        }

        Ok(Self {
            name,
            cleaned_up: false,
        })
    }

    /// Send a tmux key name (e.g., "Enter", "Escape", "C-u").
    pub fn send_keys(&self, keys: &str) -> Result<()> {
        self.run_tmux_cmd(&format!("tmux send-keys -t {} {}", self.name, keys))
    }

    /// Send literal text (no key-name interpretation).
    pub fn send_keys_literal(&self, text: &str) -> Result<()> {
        self.run_tmux_cmd(&format!(
            "tmux send-keys -t {} -l {}",
            self.name,
            shell_escape(text)
        ))
    }

    /// Run a tmux command through bash to ensure identical behaviour
    /// to running tmux from an interactive shell.  Direct `Command::new("tmux")`
    /// invocations sometimes fail to deliver keystrokes to Ink-based TUIs.
    fn run_tmux_cmd(&self, cmd: &str) -> Result<()> {
        let output = Command::new("bash")
            .args(["-c", cmd])
            .output()
            .with_context(|| format!("Failed to run: {}", cmd))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux command failed: {}", stderr.trim());
        }
        Ok(())
    }

    /// Capture the current visible pane content.
    pub fn capture_pane(&self) -> Result<String> {
        let output = Command::new("tmux")
            .args(["capture-pane", "-p", "-t", &self.name])
            .output()
            .context("tmux capture-pane failed")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux capture-pane error: {}", stderr.trim());
        }
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        // Trim trailing empty lines (tmux pads to window height)
        Ok(text.trim_end().to_string())
    }

    /// Poll capture_pane until matcher returns true or timeout.
    /// If `stabilize` is true, requires BOTH the matcher to match AND content to be
    /// stable for 3 consecutive polls before returning success.
    pub fn wait_for<F: Fn(&str) -> bool>(
        &self,
        matcher: F,
        timeout: Duration,
        interval: Duration,
        stabilize: bool,
        verbose: bool,
    ) -> Result<String> {
        let start = Instant::now();
        let mut last_content = String::new();
        let mut stable_count = 0;
        let mut matcher_matched = false;

        loop {
            if pty::is_shutdown_requested() {
                bail!("[timeout] Interrupted by shutdown signal");
            }

            if start.elapsed() > timeout {
                if verbose {
                    eprintln!(
                        "[verbose] Timeout. Last captured content:\n{}",
                        last_content
                    );
                }
                bail!(
                    "[timeout] Timed out after {:.0}s waiting for expected content",
                    timeout.as_secs_f64()
                );
            }

            let content = self.capture_pane()?;

            if matcher(&content) {
                if !stabilize {
                    return Ok(content);
                }
                matcher_matched = true;
            }

            if stabilize {
                if content == last_content && !content.trim().is_empty() {
                    stable_count += 1;
                    if stable_count >= 3 && matcher_matched {
                        return Ok(content);
                    }
                } else {
                    stable_count = 0;
                }
            }

            // Check if the tmux session still exists (process may have exited)
            if !matcher_matched && !self.session_exists() {
                let exit_content = self.capture_pane().unwrap_or_else(|_| last_content.clone());
                let tail = if exit_content.len() > 4000 {
                    exit_content[exit_content.len() - 4000..].to_string()
                } else {
                    exit_content
                };
                bail!(
                    "[timeout] Process exited before expected content. Last output:\n{}",
                    tail
                );
            }

            last_content = content;
            thread::sleep(interval);
        }
    }

    /// Wait for the pane content to stabilize (3 consecutive identical captures).
    pub fn wait_for_stable(
        &self,
        timeout: Duration,
        interval: Duration,
        verbose: bool,
    ) -> Result<String> {
        self.wait_for(|_| true, timeout, interval, true, verbose)
    }

    fn session_exists(&self) -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", &self.name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn cleanup(&mut self) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        self.cleanup();
    }
}
