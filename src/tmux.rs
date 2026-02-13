use anyhow::{bail, Context, Result};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

pub struct TmuxSession {
    pub name: String,
}

impl TmuxSession {
    pub fn new() -> Result<Self> {
        // Check tmux is available
        Command::new("tmux")
            .arg("-V")
            .output()
            .context("tmux not found. Install it with: brew install tmux (macOS) or apt install tmux (Linux)")?;

        let name = format!("agentusage-{}", std::process::id());

        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", &name, "-x", "200", "-y", "50"])
            .status()
            .context("Failed to create tmux session")?;

        if !status.success() {
            bail!("tmux new-session failed");
        }

        Ok(Self { name })
    }

    pub fn send_keys(&self, keys: &str) -> Result<()> {
        let status = Command::new("tmux")
            .args(["send-keys", "-t", &self.name, keys])
            .status()
            .context("Failed to send keys to tmux session")?;

        if !status.success() {
            bail!("tmux send-keys failed");
        }

        Ok(())
    }

    pub fn send_keys_literal(&self, keys: &str) -> Result<()> {
        let status = Command::new("tmux")
            .args(["send-keys", "-t", &self.name, "-l", keys])
            .status()
            .context("Failed to send literal keys to tmux session")?;

        if !status.success() {
            bail!("tmux send-keys failed");
        }

        Ok(())
    }

    pub fn capture_pane(&self) -> Result<String> {
        let output = Command::new("tmux")
            .args(["capture-pane", "-t", &self.name, "-p", "-S", "-50"])
            .output()
            .context("Failed to capture tmux pane")?;

        if !output.status.success() {
            bail!("tmux capture-pane failed");
        }

        // strip-ansi-escapes in case -p doesn't fully strip them
        let raw = output.stdout;
        let stripped = strip_ansi_escapes::strip(&raw);
        Ok(String::from_utf8_lossy(&stripped).to_string())
    }

    /// Poll capture_pane until matcher returns true or timeout.
    /// If `stabilize` is true, also returns when content is stable for 3 consecutive polls
    /// (useful for prompt detection where we don't know the exact content).
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

        loop {
            if start.elapsed() > timeout {
                if verbose {
                    eprintln!("[verbose] Timeout. Last captured content:\n{}", last_content);
                }
                bail!(
                    "Timed out after {:.0}s waiting for expected content",
                    timeout.as_secs_f64()
                );
            }

            let content = self.capture_pane()?;

            if matcher(&content) {
                return Ok(content);
            }

            if stabilize {
                if content == last_content && !content.trim().is_empty() {
                    stable_count += 1;
                    if stable_count >= 3 {
                        return Ok(content);
                    }
                } else {
                    stable_count = 0;
                }
            }

            last_content = content;
            thread::sleep(interval);
        }
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .status();
    }
}
