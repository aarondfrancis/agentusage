use anyhow::{bail, Context, Result};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Global shutdown flag, set by Ctrl+C handler.
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Dedicated tmux socket name to isolate from user's tmux server.
const SOCKET_NAME: &str = "agentusage";

pub struct TmuxSession {
    pub name: String,
}

impl TmuxSession {
    pub fn new(directory: Option<&str>) -> Result<Self> {
        // Check tmux is available
        Command::new("tmux")
            .arg("-V")
            .output()
            .context("tmux not found. Install it with: brew install tmux (macOS) or apt install tmux (Linux)")?;

        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let name = format!("agentusage-{}-{}", std::process::id(), nanos);

        let mut args = vec!["-L", SOCKET_NAME, "new-session", "-d", "-s", &name, "-x", "200", "-y", "50"];
        if let Some(dir) = directory {
            args.push("-c");
            args.push(dir);
        }

        let status = Command::new("tmux")
            .args(&args)
            .status()
            .context("Failed to create tmux session")?;

        if !status.success() {
            bail!("tmux new-session failed");
        }

        Ok(Self { name })
    }

    pub fn send_keys(&self, keys: &str) -> Result<()> {
        let status = Command::new("tmux")
            .args(["-L", SOCKET_NAME, "send-keys", "-t", &self.name, keys])
            .status()
            .context("Failed to send keys to tmux session")?;

        if !status.success() {
            bail!("tmux send-keys failed");
        }

        Ok(())
    }

    pub fn send_keys_literal(&self, keys: &str) -> Result<()> {
        let status = Command::new("tmux")
            .args(["-L", SOCKET_NAME, "send-keys", "-t", &self.name, "-l", keys])
            .status()
            .context("Failed to send literal keys to tmux session")?;

        if !status.success() {
            bail!("tmux send-keys failed");
        }

        Ok(())
    }

    pub fn capture_pane(&self) -> Result<String> {
        let output = Command::new("tmux")
            .args(["-L", SOCKET_NAME, "capture-pane", "-t", &self.name, "-p", "-S", "-"])
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
            if SHUTDOWN.load(Ordering::Relaxed) {
                bail!("[timeout] Interrupted by shutdown signal");
            }

            if start.elapsed() > timeout {
                if verbose {
                    eprintln!("[verbose] Timeout. Last captured content:\n{}", last_content);
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

            last_content = content;
            thread::sleep(interval);
        }
    }

    /// Wait for the pane content to stabilize (3 consecutive identical captures).
    /// Uses a permissive matcher that accepts any content.
    pub fn wait_for_stable(&self, timeout: Duration, interval: Duration, verbose: bool) -> Result<String> {
        // Use a matcher that always returns true so stabilize logic drives the return
        self.wait_for(|_| true, timeout, interval, true, verbose)
    }

    /// Kill all stale agentusage sessions on the dedicated socket.
    pub fn kill_all_stale_sessions() {
        // Kill sessions on our dedicated socket
        if let Ok(output) = Command::new("tmux")
            .args(["-L", SOCKET_NAME, "list-sessions", "-F", "#{session_name}"])
            .output()
        {
            if output.status.success() {
                let sessions = String::from_utf8_lossy(&output.stdout);
                let mut count = 0;
                for session in sessions.lines() {
                    let session = session.trim();
                    if session.starts_with("agentusage-") {
                        let _ = Command::new("tmux")
                            .args(["-L", SOCKET_NAME, "kill-session", "-t", session])
                            .status();
                        count += 1;
                    }
                }
                if count > 0 {
                    eprintln!("Killed {} stale session(s) on agentusage socket.", count);
                }
            }
        }

        // Also check default socket for pre-upgrade sessions
        if let Ok(output) = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
        {
            if output.status.success() {
                let sessions = String::from_utf8_lossy(&output.stdout);
                let mut count = 0;
                for session in sessions.lines() {
                    let session = session.trim();
                    if session.starts_with("agentusage-") {
                        let _ = Command::new("tmux")
                            .args(["kill-session", "-t", session])
                            .status();
                        count += 1;
                    }
                }
                if count > 0 {
                    eprintln!("Killed {} stale session(s) on default socket.", count);
                }
            }
        }

        eprintln!("Cleanup complete.");
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        match Command::new("tmux")
            .args(["-L", SOCKET_NAME, "kill-session", "-t", &self.name])
            .status()
        {
            Ok(status) if !status.success() => {
                eprintln!("Warning: failed to kill tmux session '{}'", self.name);
            }
            Err(e) => {
                eprintln!("Warning: failed to kill tmux session '{}': {}", self.name, e);
            }
            _ => {}
        }
    }
}
