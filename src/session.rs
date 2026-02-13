use crate::pty;
use crate::pty::PtySession;
use anyhow::Result;
use std::time::Duration;

pub struct Session {
    inner: PtySession,
}

pub struct SessionLaunch<'a> {
    pub binary: &'a str,
    pub args: &'a [&'a str],
}

impl Session {
    /// Create a new PTY-backed session.
    pub fn new(directory: Option<&str>, _verbose: bool, launch: SessionLaunch<'_>) -> Result<Self> {
        Ok(Self {
            inner: PtySession::new(directory, launch.binary, launch.args)?,
        })
    }

    pub fn backend_name(&self) -> &'static str {
        "openpty"
    }

    pub fn send_keys(&mut self, keys: &str) -> Result<()> {
        self.inner.send_keys(keys)
    }

    pub fn send_keys_literal(&mut self, keys: &str) -> Result<()> {
        self.inner.send_keys_literal(keys)
    }

    pub fn capture_pane(&mut self) -> Result<String> {
        self.inner.capture_pane()
    }

    pub fn wait_for<F: Fn(&str) -> bool>(
        &mut self,
        matcher: F,
        timeout: Duration,
        interval: Duration,
        stabilize: bool,
        verbose: bool,
    ) -> Result<String> {
        self.inner
            .wait_for(matcher, timeout, interval, stabilize, verbose)
    }

    pub fn wait_for_stable(
        &mut self,
        timeout: Duration,
        interval: Duration,
        verbose: bool,
    ) -> Result<String> {
        self.inner.wait_for_stable(timeout, interval, verbose)
    }

    /// Kill sessions registered by the current process (used by Ctrl+C handler).
    pub fn kill_registered_sessions() {
        pty::kill_registered_sessions();
    }

    /// Kill any currently registered PTY groups.
    pub fn kill_all_stale_sessions() {
        pty::kill_registered_sessions();
    }
}
