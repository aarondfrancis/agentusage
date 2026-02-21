use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Registry of active PTY process groups for targeted Ctrl+C cleanup.
static PROCESS_GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());
/// Global shutdown flag, set by Ctrl+C handler.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

const MAX_BUFFER_BYTES: usize = 1_000_000;

/// Terminal queries we respond to, enabling Ink-based TUIs (Gemini) to
/// complete their initialisation handshake without blocking indefinitely.
const CURSOR_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_RESPONSE: &[u8] = b"\x1b[1;1R";
/// Primary Device Attributes (DA1): `\x1b[c`
const DA1_QUERY: &[u8] = b"\x1b[c";
const DA1_RESPONSE: &[u8] = b"\x1b[?1;2c"; // VT100 with AVO
/// Device Status Report (DSR): `\x1b[5n`
const DSR_QUERY: &[u8] = b"\x1b[5n";
const DSR_RESPONSE: &[u8] = b"\x1b[0n"; // terminal OK

fn register_group(pgid: i32) {
    if let Ok(mut groups) = PROCESS_GROUPS.lock() {
        groups.push(pgid);
    }
}

fn unregister_group(pgid: i32) {
    if let Ok(mut groups) = PROCESS_GROUPS.lock() {
        groups.retain(|g| *g != pgid);
    }
}

fn kill_group(pgid: i32, signal: i32) {
    // Negative PID targets the process group.
    let _ = unsafe { libc::kill(-pgid, signal) };
}

/// Kill all PTY-backed groups registered by this process.
pub fn kill_registered_sessions() {
    let groups = if let Ok(groups) = PROCESS_GROUPS.lock() {
        groups.clone()
    } else {
        Vec::new()
    };

    for pgid in &groups {
        kill_group(*pgid, libc::SIGTERM);
    }

    thread::sleep(Duration::from_millis(300));

    for pgid in &groups {
        kill_group(*pgid, libc::SIGKILL);
    }
}

/// Signal long-running wait loops to stop quickly (used by Ctrl+C handler).
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Clear the global shutdown flag.
pub fn clear_shutdown() {
    SHUTDOWN.store(false, Ordering::SeqCst);
}

fn map_special_key(keys: &str) -> &str {
    match keys {
        "Enter" => "\r",
        "Tab" => "\t",
        "Esc" => "\u{1b}",
        "Up" => "\u{1b}[A",
        "Down" => "\u{1b}[B",
        "Right" => "\u{1b}[C",
        "Left" => "\u{1b}[D",
        _ => keys,
    }
}

/// Scan for `query` in the combined tail+chunk stream, updating the tail
/// buffer for cross-chunk detection.  Returns true if the query was found.
fn detect_query_in_stream(tail: &mut Vec<u8>, chunk: &[u8], query: &[u8]) -> bool {
    let mut combined = Vec::with_capacity(tail.len() + chunk.len());
    combined.extend_from_slice(tail);
    combined.extend_from_slice(chunk);

    let found = combined.windows(query.len()).any(|window| window == query);

    let tail_len = query.len().saturating_sub(1);
    tail.clear();
    if tail_len > 0 {
        if combined.len() >= tail_len {
            tail.extend_from_slice(&combined[combined.len() - tail_len..]);
        } else {
            tail.extend_from_slice(&combined);
        }
    }

    found
}

pub struct PtySession {
    pub name: String,
    master_fd: RawFd,
    child: Child,
    process_group: Option<i32>,
    buffer: Vec<u8>,
    cursor_query_tail: Vec<u8>,
    da1_query_tail: Vec<u8>,
    dsr_query_tail: Vec<u8>,
    cleaned_up: bool,
}

impl PtySession {
    pub fn new(directory: Option<&str>, binary: &str, args: &[&str]) -> Result<Self> {
        let mut master_fd: libc::c_int = -1;
        let mut slave_fd: libc::c_int = -1;
        let mut win = libc::winsize {
            ws_row: 50,
            ws_col: 200,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let win_ptr = std::ptr::addr_of_mut!(win);

        // SAFETY: openpty initializes two FDs and optional terminal sizing.
        let rc = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                win_ptr,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            bail!("openpty failed: {}", err);
        }

        // Make reads non-blocking so polling loops never hang.
        // SAFETY: fcntl is called on a valid FD returned by openpty.
        let flags = unsafe { libc::fcntl(master_fd, libc::F_GETFL) };
        if flags < 0 {
            let err = std::io::Error::last_os_error();
            // SAFETY: closing an FD from openpty.
            let _ = unsafe { libc::close(master_fd) };
            // SAFETY: closing an FD from openpty.
            let _ = unsafe { libc::close(slave_fd) };
            bail!("fcntl(F_GETFL) failed: {}", err);
        }
        // SAFETY: fcntl on a valid FD; OR-ing current flags with O_NONBLOCK is standard.
        let set_rc = unsafe { libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if set_rc != 0 {
            let err = std::io::Error::last_os_error();
            // SAFETY: closing an FD from openpty.
            let _ = unsafe { libc::close(master_fd) };
            // SAFETY: closing an FD from openpty.
            let _ = unsafe { libc::close(slave_fd) };
            bail!("fcntl(F_SETFL) failed: {}", err);
        }

        // SAFETY: slave_fd ownership moves into File.
        let slave_file = unsafe { File::from_raw_fd(slave_fd) };
        let slave_out = match slave_file.try_clone() {
            Ok(fd) => fd,
            Err(e) => {
                // SAFETY: close valid master FD on setup failure.
                let _ = unsafe { libc::close(master_fd) };
                return Err(e).context("Failed to clone slave PTY fd");
            }
        };
        let slave_err = match slave_file.try_clone() {
            Ok(fd) => fd,
            Err(e) => {
                // SAFETY: close valid master FD on setup failure.
                let _ = unsafe { libc::close(master_fd) };
                return Err(e).context("Failed to clone slave PTY fd");
            }
        };

        let mut cmd = Command::new(binary);
        cmd.args(args);
        if let Some(dir) = directory {
            cmd.current_dir(dir);
            cmd.env("PWD", dir);
        }
        if std::env::var_os("TERM").is_none() {
            cmd.env("TERM", "xterm-256color");
        }
        if std::env::var_os("COLORTERM").is_none() {
            cmd.env("COLORTERM", "truecolor");
        }
        if std::env::var_os("LANG").is_none() {
            cmd.env("LANG", "en_US.UTF-8");
        }
        if std::env::var_os("CI").is_none() {
            cmd.env("CI", "0");
        }
        let preexec_slave_fd = slave_fd;
        // Make the child a session leader with the slave PTY as controlling terminal.
        // This matches how interactive TUIs expect to be launched.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(preexec_slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.stdin(Stdio::from(slave_file));
        cmd.stdout(Stdio::from(slave_out));
        cmd.stderr(Stdio::from(slave_err));

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                // SAFETY: close valid master FD on launch failure.
                let _ = unsafe { libc::close(master_fd) };
                return Err(e).with_context(|| format!("Failed to launch '{}' in PTY", binary));
            }
        };
        let child_pid = child.id() as i32;

        // Best-effort process-group tracking; this lets us tear down child trees reliably.
        // If setpgid fails (e.g. child already became a session leader in pre_exec), use getpgid.
        let mut process_group = None;
        // SAFETY: setpgid on a live child PID.
        if unsafe { libc::setpgid(child_pid, child_pid) } == 0 {
            process_group = Some(child_pid);
        } else {
            // SAFETY: getpgid on a live child PID.
            let pgid = unsafe { libc::getpgid(child_pid) };
            if pgid > 0 {
                process_group = Some(pgid);
            }
        }
        if let Some(pgid) = process_group {
            register_group(pgid);
        }

        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let name = format!("agentusage-pty-{}-{}-{}", binary, std::process::id(), nanos);

        Ok(Self {
            name,
            master_fd,
            child,
            process_group,
            buffer: Vec::with_capacity(64 * 1024),
            cursor_query_tail: Vec::new(),
            da1_query_tail: Vec::new(),
            dsr_query_tail: Vec::new(),
            cleaned_up: false,
        })
    }

    pub fn send_keys(&self, keys: &str) -> Result<()> {
        self.write_all_to_master(map_special_key(keys).as_bytes())
    }

    pub fn send_keys_literal(&self, keys: &str) -> Result<()> {
        self.write_all_to_master(keys.as_bytes())
    }

    pub fn capture_pane(&mut self) -> Result<String> {
        self.read_available();
        let stripped = strip_ansi_escapes::strip(&self.buffer);
        Ok(String::from_utf8_lossy(&stripped).to_string())
    }

    /// Poll capture_pane until matcher returns true or timeout.
    /// If `stabilize` is true, requires BOTH the matcher to match AND content to be
    /// stable for 3 consecutive polls before returning success.
    pub fn wait_for<F: Fn(&str) -> bool>(
        &mut self,
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

            match self.child.try_wait() {
                Ok(Some(status)) if !matcher_matched => {
                    let status_text = status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string());
                    let exit_content = self.capture_pane().unwrap_or_else(|_| last_content.clone());
                    let tail = if exit_content.len() > 4000 {
                        exit_content[exit_content.len() - 4000..].to_string()
                    } else {
                        exit_content
                    };
                    if verbose && !tail.trim().is_empty() {
                        eprintln!("[verbose] Process exited. Captured output:\n{}", tail);
                    }
                    bail!(
                        "[timeout] Process exited before expected content (status: {}){}",
                        status_text,
                        if tail.trim().is_empty() {
                            String::new()
                        } else {
                            format!(". Last output:\n{}", tail)
                        }
                    );
                }
                _ => {}
            }

            last_content = content;
            thread::sleep(interval);
        }
    }

    /// Wait for the pane content to stabilize (3 consecutive identical captures).
    /// Uses a permissive matcher that accepts any content.
    pub fn wait_for_stable(
        &mut self,
        timeout: Duration,
        interval: Duration,
        verbose: bool,
    ) -> Result<String> {
        self.wait_for(|_| true, timeout, interval, true, verbose)
    }

    fn read_available(&mut self) {
        loop {
            let mut tmp = [0u8; 8192];
            // SAFETY: read from valid master PTY FD into stack buffer.
            let n = unsafe {
                libc::read(
                    self.master_fd,
                    tmp.as_mut_ptr() as *mut libc::c_void,
                    tmp.len(),
                )
            };
            if n > 0 {
                let chunk = &tmp[..n as usize];
                self.respond_to_terminal_queries(chunk);
                self.buffer.extend_from_slice(chunk);
                self.trim_buffer();
                continue;
            }
            if n == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => break,
                _ => break,
            }
        }
    }

    fn trim_buffer(&mut self) {
        if self.buffer.len() > MAX_BUFFER_BYTES {
            let drop_len = self.buffer.len() - MAX_BUFFER_BYTES;
            self.buffer.drain(..drop_len);
        }
    }

    fn respond_to_terminal_queries(&mut self, chunk: &[u8]) {
        if detect_query_in_stream(&mut self.cursor_query_tail, chunk, CURSOR_QUERY) {
            let _ = self.write_all_to_master(CURSOR_RESPONSE);
        }
        if detect_query_in_stream(&mut self.da1_query_tail, chunk, DA1_QUERY) {
            let _ = self.write_all_to_master(DA1_RESPONSE);
        }
        if detect_query_in_stream(&mut self.dsr_query_tail, chunk, DSR_QUERY) {
            let _ = self.write_all_to_master(DSR_RESPONSE);
        }
    }

    fn write_all_to_master(&self, data: &[u8]) -> Result<()> {
        if self.master_fd < 0 {
            bail!("PTY is not available");
        }

        let mut offset = 0usize;
        let mut retries = 0u32;

        while offset < data.len() {
            // SAFETY: writing byte slice to valid PTY master FD.
            let written = unsafe {
                libc::write(
                    self.master_fd,
                    data[offset..].as_ptr() as *const libc::c_void,
                    data.len() - offset,
                )
            };
            if written > 0 {
                offset += written as usize;
                retries = 0;
                continue;
            }
            if written == 0 {
                break;
            }

            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                    retries += 1;
                    if retries > 200 {
                        bail!("write to PTY would block");
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                _ => bail!("write to PTY failed: {}", err),
            }
        }

        Ok(())
    }

    fn cleanup(&mut self) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;

        let _ = self.send_keys_literal("/exit\n");

        if self.master_fd >= 0 {
            // SAFETY: close valid master FD once.
            let _ = unsafe { libc::close(self.master_fd) };
            self.master_fd = -1;
        }

        let pid = self.child.id() as i32;

        if let Some(pgid) = self.process_group {
            kill_group(pgid, libc::SIGTERM);
        } else {
            // SAFETY: signal child PID directly as fallback.
            let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }

        let still_running = matches!(self.child.try_wait(), Ok(None));
        if still_running {
            if let Some(pgid) = self.process_group {
                kill_group(pgid, libc::SIGKILL);
            } else {
                // SAFETY: force kill child PID as fallback.
                let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            let _ = self.child.wait();
        }

        if let Some(pgid) = self.process_group.take() {
            unregister_group(pgid);
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ShutdownGuard;

    impl Drop for ShutdownGuard {
        fn drop(&mut self) {
            clear_shutdown();
        }
    }

    #[test]
    fn test_map_special_key_sequences() {
        assert_eq!(map_special_key("Enter"), "\r");
        assert_eq!(map_special_key("Tab"), "\t");
        assert_eq!(map_special_key("Esc"), "\u{1b}");
        assert_eq!(map_special_key("Up"), "\u{1b}[A");
        assert_eq!(map_special_key("Down"), "\u{1b}[B");
        assert_eq!(map_special_key("Right"), "\u{1b}[C");
        assert_eq!(map_special_key("Left"), "\u{1b}[D");
        assert_eq!(map_special_key("literal"), "literal");
    }

    #[test]
    fn test_detect_cursor_query_in_single_chunk() {
        let mut tail = Vec::new();
        let found = detect_query_in_stream(&mut tail, b"abc\x1b[6ndef", CURSOR_QUERY);
        assert!(found);
        assert_eq!(tail, b"def");
    }

    #[test]
    fn test_detect_cursor_query_split_across_chunks() {
        let mut tail = Vec::new();
        let first = detect_query_in_stream(&mut tail, b"hello\x1b[", CURSOR_QUERY);
        assert!(!first);
        let second = detect_query_in_stream(&mut tail, b"6nworld", CURSOR_QUERY);
        assert!(second);
        assert_eq!(tail, b"rld");
    }

    #[test]
    fn test_detect_da1_query() {
        let mut tail = Vec::new();
        let found = detect_query_in_stream(&mut tail, b"prefix\x1b[csuffix", DA1_QUERY);
        assert!(found);
    }

    #[test]
    fn test_detect_da1_no_false_positive_on_cursor_query() {
        // \x1b[6n should NOT match DA1 (\x1b[c)
        let mut tail = Vec::new();
        let found = detect_query_in_stream(&mut tail, b"\x1b[6n", DA1_QUERY);
        assert!(!found);
    }

    #[test]
    fn test_detect_dsr_query() {
        let mut tail = Vec::new();
        let found = detect_query_in_stream(&mut tail, b"\x1b[5n", DSR_QUERY);
        assert!(found);
    }

    #[test]
    fn test_new_registers_and_drop_unregisters_process_group() -> Result<()> {
        clear_shutdown();
        let _guard = ShutdownGuard;
        let session = PtySession::new(None, "sh", &["-c", "sleep 1"])?;
        let pgid = session.process_group.expect("expected process group");

        {
            let groups = PROCESS_GROUPS
                .lock()
                .expect("process-group registry should lock");
            assert!(groups.contains(&pgid));
        }

        drop(session);

        {
            let groups = PROCESS_GROUPS
                .lock()
                .expect("process-group registry should lock");
            assert!(!groups.contains(&pgid));
        }
        Ok(())
    }

    #[test]
    fn test_wait_for_stops_on_shutdown_signal() -> Result<()> {
        clear_shutdown();
        let _guard = ShutdownGuard;
        let mut session = PtySession::new(None, "sh", &["-c", "sleep 5"])?;

        let signaler = thread::spawn(|| {
            thread::sleep(Duration::from_millis(120));
            request_shutdown();
        });

        let err = session
            .wait_for(
                |_| false,
                Duration::from_secs(2),
                Duration::from_millis(40),
                false,
                false,
            )
            .expect_err("wait should stop when shutdown is requested");

        let _ = signaler.join();
        let text = format!("{:#}", err);
        assert!(text.contains("Interrupted by shutdown signal"));
        Ok(())
    }
}
