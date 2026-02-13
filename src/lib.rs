#![deny(warnings)]

pub mod dialog;
pub mod parser;
pub mod pty;
pub mod session;
pub mod types;

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use dialog::{
    detect_claude_dialog, detect_codex_dialog, detect_gemini_dialog, dialog_error_message,
    dismiss_dialog,
};
use parser::{parse_claude_output, parse_codex_output, parse_gemini_output};
use session::{Session, SessionLaunch};
use types::DialogKind;

pub use types::{ApprovalPolicy, PercentKind, UsageData, UsageEntry};

/// Library-friendly configuration for running usage checks.
pub struct UsageConfig {
    pub timeout: u64,
    pub verbose: bool,
    pub approval_policy: ApprovalPolicy,
    pub directory: Option<String>,
}

/// Results from checking all providers.
pub struct AllResults {
    pub results: Vec<UsageData>,
    /// Provider name → error message (raw, may contain internal tags like `[timeout]`).
    pub warnings: BTreeMap<String, String>,
}

pub fn check_command_exists(cmd: &str) -> Result<()> {
    match Command::new(cmd).arg("--version").output() {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "[tool-missing] {} CLI not found. Make sure it is installed and on your PATH.",
                cmd
            );
        }
        Err(_) => {
            // Binary exists but --version might not be supported; that's fine
            Ok(())
        }
    }
}

/// Handle dialog detection and policy for a provider.
/// Returns Ok(true) if a dialog was found and dismissed (caller should retry wait),
/// Ok(false) if no dialog found, or Err if dialog found and policy is Fail / not dismissible.
fn handle_dialog_check<F>(
    session: &mut Session,
    detect_fn: F,
    provider: &str,
    policy: ApprovalPolicy,
    verbose: bool,
) -> Result<bool>
where
    F: Fn(&str) -> Option<DialogKind>,
{
    let content = session.capture_pane()?;
    if let Some(kind) = detect_fn(&content) {
        if verbose {
            eprintln!("[verbose] Dialog detected: {:?}", kind);
        }

        match policy {
            ApprovalPolicy::Fail => {
                bail!("[timeout] {}", dialog_error_message(&kind, provider));
            }
            ApprovalPolicy::Accept => {
                let dismissed = dismiss_dialog(&kind, session)?;
                if !dismissed {
                    bail!("[timeout] {}", dialog_error_message(&kind, provider));
                }
                if verbose {
                    eprintln!("[verbose] Dialog dismissed, retrying...");
                }
                Ok(true)
            }
        }
    } else {
        Ok(false)
    }
}

/// Return whichever UsageData has more entries.
fn pick_richer(a: UsageData, b: UsageData) -> UsageData {
    if a.entries.len() >= b.entries.len() {
        a
    } else {
        b
    }
}

fn looks_like_codex_update_prompt(content: &str) -> bool {
    let lower = content.to_lowercase();
    lower.contains("update available") && lower.contains("codex")
}

fn content_tail(content: &str, max_chars: usize) -> String {
    let mut chars: Vec<char> = content.chars().rev().take(max_chars).collect();
    chars.reverse();
    chars.into_iter().collect()
}

fn normalized_no_whitespace_lower(content: &str) -> String {
    content
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

pub fn run_claude(config: &UsageConfig) -> Result<UsageData> {
    check_command_exists("claude")?;

    let mut session = Session::new(
        config.directory.as_deref(),
        config.verbose,
        SessionLaunch {
            binary: "claude",
            args: &["--allowed-tools", ""],
        },
    )?;
    let poll_interval = Duration::from_millis(500);
    let prompt_timeout = Duration::from_secs(30);
    let data_timeout = Duration::from_secs(config.timeout);

    if config.verbose {
        eprintln!(
            "[verbose] Created {} session for claude",
            session.backend_name()
        );
    }

    if config.verbose {
        eprintln!("[verbose] Launched claude, waiting for prompt...");
    }

    let prompt_result = session.wait_for(
        |content| {
            let t = content.trim();
            t.contains('>') || t.contains('❯') || t.contains("Tips")
        },
        prompt_timeout,
        poll_interval,
        true,
        config.verbose,
    );

    if let Err(e) = prompt_result {
        // Check for dialogs before giving up
        if handle_dialog_check(
            &mut session,
            detect_claude_dialog,
            "claude",
            config.approval_policy,
            config.verbose,
        )? {
            // Dialog dismissed, retry waiting for prompt
            session
                .wait_for(
                    |content| {
                        let t = content.trim();
                        t.contains('>') || t.contains('❯') || t.contains("Tips")
                    },
                    prompt_timeout,
                    poll_interval,
                    true,
                    config.verbose,
                )
                .context(
                    "[timeout] Timed out waiting for Claude prompt after dismissing dialog.",
                )?;
        } else {
            return Err(e.context(
                "Timed out waiting for Claude prompt. Is claude authenticated? Try running 'claude' manually."
            ));
        }
    }

    // Wait for TUI to stabilize instead of fixed sleep
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, config.verbose);

    if config.verbose {
        let content = session.capture_pane()?;
        eprintln!("[verbose] Prompt detected. Current pane:\n{}", content);
    }

    // Claude's newer UI is most stable via `/usage`; `/status` now opens a tabbed screen
    // where `Config` may be selected first.
    session.send_keys("Esc")?;
    std::thread::sleep(Duration::from_millis(120));
    session.send_keys_literal("/usage")?;
    std::thread::sleep(Duration::from_millis(250));
    session.send_keys("Enter")?;

    if config.verbose {
        eprintln!("[verbose] Sent /usage + Enter, waiting for usage data...");
    }

    let pct_re = regex::Regex::new(r"\d+(?:\.\d+)?%\s*used")?;
    let usage_start = std::time::Instant::now();
    let mut last_enter = usage_start
        .checked_sub(Duration::from_secs(1))
        .unwrap_or(usage_start);
    let mut content = String::new();
    let mut usage_ready = false;

    while usage_start.elapsed() < data_timeout {
        content = session.capture_pane()?;
        let normalized = normalized_no_whitespace_lower(&content);

        if pct_re.is_match(&content) {
            usage_ready = true;
            break;
        }

        // If Claude opened a prompt/menu (update/auth/etc), handle it and keep going.
        if handle_dialog_check(
            &mut session,
            detect_claude_dialog,
            "claude",
            config.approval_policy,
            config.verbose,
        )? {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }

        // Command palette hint rows sometimes require one more Enter to execute `/usage`.
        if normalized.contains("showplanusagelimits")
            || normalized.contains("showplan")
            || normalized.contains("/usage")
        {
            session.send_keys("Enter")?;
            last_enter = std::time::Instant::now();
            std::thread::sleep(Duration::from_millis(180));
            continue;
        }

        // Nudge the TUI occasionally while waiting for usage panels to render.
        if !pct_re.is_match(&content) && last_enter.elapsed() >= Duration::from_millis(850) {
            session.send_keys("Enter")?;
            last_enter = std::time::Instant::now();
        }

        std::thread::sleep(poll_interval);
    }

    if !usage_ready {
        if config.verbose {
            eprintln!(
                "[verbose] /usage did not render in time; falling back to /status usage tab navigation"
            );
        }
        session.send_keys("Esc")?;
        std::thread::sleep(Duration::from_millis(120));
        session.send_keys_literal("/status")?;
        std::thread::sleep(Duration::from_millis(300));
        session.send_keys("Enter")?;

        // Wait for the status screen tab bar and then move right toward Usage.
        session
            .wait_for(
                |content| {
                    let tail = content_tail(content, 4000);
                    tail.contains("Status") && tail.contains("Config") && tail.contains("Usage")
                },
                Duration::from_secs(15),
                poll_interval,
                false,
                config.verbose,
            )
            .context("[timeout] Timed out waiting for status screen")?;

        for _ in 0..4 {
            let screen = session.capture_pane()?;
            if pct_re.is_match(&screen) {
                content = screen;
                usage_ready = true;
                break;
            }
            session.send_keys("Right")?;
            std::thread::sleep(Duration::from_millis(250));
        }

        if !usage_ready {
            content = session
                .wait_for(
                    |screen| pct_re.is_match(screen),
                    data_timeout,
                    poll_interval,
                    false,
                    config.verbose,
                )
                .context(
                    "[timeout] Timed out waiting for usage data. Check your internet connection.",
                )?;
        }
    }

    // Wait for TUI to stabilize instead of fixed sleep
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, config.verbose);

    let final_content = session.capture_pane()?;

    if config.verbose {
        eprintln!("[verbose] Raw captured text:\n{}", final_content);
    }

    let data_final = parse_claude_output(&final_content)?;
    let data_early = parse_claude_output(&content)?;
    let data = pick_richer(data_final, data_early);

    if data.entries.is_empty() {
        bail!("[parse-failure] No usage data found in captured output. Run with --verbose to see raw text.");
    }

    Ok(data)
}

pub fn run_codex(config: &UsageConfig) -> Result<UsageData> {
    check_command_exists("codex")?;

    let mut session = Session::new(
        config.directory.as_deref(),
        config.verbose,
        SessionLaunch {
            binary: "codex",
            args: &["-s", "read-only", "-a", "untrusted"],
        },
    )?;
    let poll_interval = Duration::from_millis(500);
    let prompt_timeout = Duration::from_secs(30);
    let data_timeout = Duration::from_secs(config.timeout);

    if config.verbose {
        eprintln!(
            "[verbose] Created {} session for codex",
            session.backend_name()
        );
    }

    if config.verbose {
        eprintln!("[verbose] Launched codex, waiting for prompt...");
    }

    // Codex prompt shows "› ..." and "? for shortcuts" at the bottom.
    // Must NOT match ">_" in the Codex banner header which appears early.
    let prompt_result = session.wait_for(
        |content| content.contains("? for shortcuts"),
        prompt_timeout,
        poll_interval,
        false,
        config.verbose,
    );

    if let Err(e) = prompt_result {
        // Check for dialogs before giving up
        if handle_dialog_check(
            &mut session,
            detect_codex_dialog,
            "codex",
            config.approval_policy,
            config.verbose,
        )? {
            // Dialog dismissed, retry waiting for prompt
            session
                .wait_for(
                    |content| content.contains("? for shortcuts"),
                    prompt_timeout,
                    poll_interval,
                    false,
                    config.verbose,
                )
                .context("[timeout] Timed out waiting for Codex prompt after dismissing dialog.")?;
        } else {
            return Err(e.context(
                "Timed out waiting for Codex prompt. Is codex authenticated? Try running 'codex' manually."
            ));
        }
    }

    // Wait for TUI to stabilize instead of fixed sleep
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, config.verbose);

    if config.verbose {
        let content = session.capture_pane()?;
        eprintln!("[verbose] Prompt detected. Current pane:\n{}", content);
    }

    // Codex /status prints inline — no autocomplete, no tabs
    session.send_keys_literal("/status")?;
    std::thread::sleep(Duration::from_millis(500));
    session.send_keys("Enter")?;

    if config.verbose {
        eprintln!("[verbose] Sent /status + Enter, waiting for usage data...");
    }

    // Wait for limit data to appear
    let limit_re = regex::Regex::new(r"\d+%\s*(left|used)")?;
    let mut content = session
        .wait_for(
            |content| limit_re.is_match(content) || looks_like_codex_update_prompt(content),
            data_timeout,
            poll_interval,
            false,
            config.verbose,
        )
        .context("[timeout] Timed out waiting for Codex usage data.")?;

    if looks_like_codex_update_prompt(&content) && !limit_re.is_match(&content) {
        if config.verbose {
            eprintln!(
                "[verbose] Codex update prompt detected, selecting Skip and retrying /status"
            );
        }
        session.send_keys("Down")?;
        std::thread::sleep(Duration::from_millis(120));
        session.send_keys("Enter")?;
        std::thread::sleep(Duration::from_millis(150));
        session.send_keys("Enter")?;
        std::thread::sleep(Duration::from_millis(200));
        session.send_keys_literal("/status")?;
        std::thread::sleep(Duration::from_millis(200));
        session.send_keys("Enter")?;

        content = session
            .wait_for(
                |content| limit_re.is_match(content),
                data_timeout,
                poll_interval,
                false,
                config.verbose,
            )
            .context(
                "[timeout] Timed out waiting for Codex usage data after dismissing update prompt.",
            )?;
    }

    // Wait for all data to render
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, config.verbose);

    let final_content = session.capture_pane()?;

    if config.verbose {
        eprintln!("[verbose] Raw captured text:\n{}", final_content);
    }

    let data_final = parse_codex_output(&final_content)?;
    let data_early = parse_codex_output(&content)?;
    let data = pick_richer(data_final, data_early);

    if data.entries.is_empty() {
        bail!("[parse-failure] No usage data found in captured output. Run with --verbose to see raw text.");
    }

    Ok(data)
}

pub fn run_gemini(config: &UsageConfig) -> Result<UsageData> {
    check_command_exists("gemini")?;

    let mut session = Session::new(
        config.directory.as_deref(),
        config.verbose,
        SessionLaunch {
            binary: "gemini",
            args: &[],
        },
    )?;
    let poll_interval = Duration::from_millis(500);
    let prompt_timeout = Duration::from_secs(30);
    let data_timeout = Duration::from_secs(config.timeout);

    if config.verbose {
        eprintln!(
            "[verbose] Created {} session for gemini",
            session.backend_name()
        );
    }

    if config.verbose {
        eprintln!("[verbose] Launched gemini, waiting for prompt...");
    }

    // Wait for Gemini prompt — match prompt OR trust dialog so we don't time out
    let prompt_result = session.wait_for(
        |content| {
            content.contains("GEMINI.md")
                || content.contains("MCP servers")
                || content.contains("gemini >")
                || content.contains("Gemini CLI")
                || content.contains("Do you trust this folder")
        },
        prompt_timeout,
        poll_interval,
        false,
        config.verbose,
    );

    if let Err(e) = prompt_result {
        // Check for dialogs before giving up
        if handle_dialog_check(
            &mut session,
            detect_gemini_dialog,
            "gemini",
            config.approval_policy,
            config.verbose,
        )? {
            // Dialog dismissed, retry waiting for prompt
            session
                .wait_for(
                    |content| {
                        content.contains("GEMINI.md")
                            || content.contains("MCP servers")
                            || content.contains("gemini >")
                            || content.contains("Gemini CLI")
                    },
                    prompt_timeout,
                    poll_interval,
                    false,
                    config.verbose,
                )
                .context(
                    "[timeout] Timed out waiting for Gemini prompt after dismissing dialog.",
                )?;
        } else {
            return Err(e.context(
                "Timed out waiting for Gemini prompt. Is gemini authenticated? Try running 'gemini' manually."
            ));
        }
    } else {
        // wait_for succeeded — check if what we matched was actually a dialog
        let content = session.capture_pane()?;
        if let Some(kind) = detect_gemini_dialog(&content) {
            if config.verbose {
                eprintln!("[verbose] Dialog detected after prompt wait: {:?}", kind);
            }
            match config.approval_policy {
                ApprovalPolicy::Fail => {
                    bail!("[timeout] {}", dialog_error_message(&kind, "gemini"));
                }
                ApprovalPolicy::Accept => {
                    let dismissed = dismiss_dialog(&kind, &mut session)?;
                    if !dismissed {
                        bail!("[timeout] {}", dialog_error_message(&kind, "gemini"));
                    }
                    if config.verbose {
                        eprintln!("[verbose] Dialog dismissed, waiting for actual prompt...");
                    }
                    // Re-wait for the actual prompt after dialog dismissal
                    session
                        .wait_for(
                            |content| {
                                content.contains("GEMINI.md")
                                    || content.contains("MCP servers")
                                    || content.contains("gemini >")
                                    || content.contains("Gemini CLI")
                            },
                            prompt_timeout,
                            poll_interval,
                            false,
                            config.verbose,
                        )
                        .context("[timeout] Timed out waiting for Gemini prompt after dismissing dialog.")?;
                }
            }
        }
    }

    // Wait for TUI to stabilize instead of fixed sleep
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, config.verbose);

    if config.verbose {
        let content = session.capture_pane()?;
        eprintln!("[verbose] Prompt detected. Current pane:\n{}", content);
    }

    // Type /stats session — Gemini uses this command, not /status
    session.send_keys_literal("/stats session")?;
    std::thread::sleep(Duration::from_millis(500));
    session.send_keys("Enter")?;

    if config.verbose {
        eprintln!("[verbose] Sent /stats session + Enter, waiting for usage data...");
    }

    // Wait for usage data to appear
    let pct_re = regex::Regex::new(r"\d+(?:\.\d+)?%\s*\(Resets?")?;
    let content = session
        .wait_for(
            |content| pct_re.is_match(content),
            data_timeout,
            poll_interval,
            false,
            config.verbose,
        )
        .context("[timeout] Timed out waiting for Gemini usage data.")?;

    // Wait for all data to render
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, config.verbose);

    let final_content = session.capture_pane()?;

    if config.verbose {
        eprintln!("[verbose] Raw captured text:\n{}", final_content);
    }

    let data_final = parse_gemini_output(&final_content)?;
    let data_early = parse_gemini_output(&content)?;
    let data = pick_richer(data_final, data_early);

    if data.entries.is_empty() {
        bail!("[parse-failure] No usage data found in captured output. Run with --verbose to see raw text.");
    }

    Ok(data)
}

pub fn run_all(config: &UsageConfig) -> AllResults {
    let mut results = Vec::new();
    let mut warnings = BTreeMap::new();

    match run_claude(config) {
        Ok(data) => results.push(data),
        Err(e) => {
            warnings.insert("claude".into(), format!("{:#}", e));
        }
    }

    match run_codex(config) {
        Ok(data) => results.push(data),
        Err(e) => {
            warnings.insert("codex".into(), format!("{:#}", e));
        }
    }

    match run_gemini(config) {
        Ok(data) => results.push(data),
        Err(e) => {
            warnings.insert("gemini".into(), format!("{:#}", e));
        }
    }

    AllResults { results, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── pick_richer ─────────────────────────────────────────────────

    #[test]
    fn test_pick_richer_first_has_more() {
        let a = UsageData {
            provider: "claude".into(),
            entries: vec![
                UsageEntry {
                    label: "session".into(),
                    percent_used: 5,
                    percent_kind: PercentKind::Used,
                    reset_info: "Resets 2pm".into(),
                    percent_remaining: 95,
                    reset_minutes: None,
                    spent: None,
                    requests: None,
                },
                UsageEntry {
                    label: "week".into(),
                    percent_used: 10,
                    percent_kind: PercentKind::Used,
                    reset_info: "Resets Feb 20".into(),
                    percent_remaining: 90,
                    reset_minutes: None,
                    spent: None,
                    requests: None,
                },
            ],
        };
        let b = UsageData {
            provider: "claude".into(),
            entries: vec![UsageEntry {
                label: "session".into(),
                percent_used: 5,
                percent_kind: PercentKind::Used,
                reset_info: "Resets 2pm".into(),
                percent_remaining: 95,
                reset_minutes: None,
                spent: None,
                requests: None,
            }],
        };
        let result = pick_richer(a, b);
        assert_eq!(result.entries.len(), 2);
    }

    #[test]
    fn test_pick_richer_second_has_more() {
        let a = UsageData {
            provider: "claude".into(),
            entries: vec![],
        };
        let b = UsageData {
            provider: "claude".into(),
            entries: vec![UsageEntry {
                label: "session".into(),
                percent_used: 5,
                percent_kind: PercentKind::Used,
                reset_info: "Resets 2pm".into(),
                percent_remaining: 95,
                reset_minutes: None,
                spent: None,
                requests: None,
            }],
        };
        let result = pick_richer(a, b);
        assert_eq!(result.entries.len(), 1);
    }

    #[test]
    fn test_pick_richer_equal_prefers_first() {
        let a = UsageData {
            provider: "claude".into(),
            entries: vec![UsageEntry {
                label: "from_a".into(),
                percent_used: 5,
                percent_kind: PercentKind::Used,
                reset_info: String::new(),
                percent_remaining: 95,
                reset_minutes: None,
                spent: None,
                requests: None,
            }],
        };
        let b = UsageData {
            provider: "claude".into(),
            entries: vec![UsageEntry {
                label: "from_b".into(),
                percent_used: 10,
                percent_kind: PercentKind::Used,
                reset_info: String::new(),
                percent_remaining: 90,
                reset_minutes: None,
                spent: None,
                requests: None,
            }],
        };
        let result = pick_richer(a, b);
        assert_eq!(result.entries[0].label, "from_a");
    }

    #[test]
    fn test_pick_richer_both_empty() {
        let a = UsageData {
            provider: "claude".into(),
            entries: vec![],
        };
        let b = UsageData {
            provider: "claude".into(),
            entries: vec![],
        };
        let result = pick_richer(a, b);
        assert!(result.entries.is_empty());
    }

    // ── check_command_exists ────────────────────────────────────────

    #[test]
    fn test_check_command_exists_valid() {
        // "ls" exists on all unix systems
        assert!(check_command_exists("ls").is_ok());
    }

    #[test]
    fn test_check_command_exists_missing() {
        let result = check_command_exists("nonexistent_tool_xyz_12345");
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("[tool-missing]"));
    }
}
