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

/// Check whether the Gemini CLI pane content indicates the prompt is
/// actually ready for input.  Only matches patterns that appear once the
/// CLI is interactive — startup-only text (identity headers, dialog
/// screens, banners) is intentionally excluded and handled separately by
/// the dialog-checking poll loop in `run_gemini`.
fn gemini_prompt_ready(content: &str) -> bool {
    // Legacy patterns (case-sensitive originals)
    if content.contains("GEMINI.md")
        || content.contains("MCP servers")
        || content.contains("gemini >")
    {
        return true;
    }

    let lower = content.to_lowercase();

    // Legacy patterns (case-insensitive variants)
    if lower.contains("gemini.md") || lower.contains("mcp servers") {
        return true;
    }

    // Ready indicator
    if lower.contains("what can i help") {
        return true;
    }

    // Bare `>` at line start (strict: entire trimmed line or `> ` prefix)
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == ">" || trimmed.starts_with("> ") {
            return true;
        }
    }

    false
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
    // Gemini v0.28+ has a long auth validation phase (spinners, loading
    // extensions, etc.) that can easily exceed 30 seconds.  We use the
    // user-configurable data timeout as the hard ceiling and separately
    // track "idle time" (no output changes) — if nothing happens for 30s
    // the CLI is likely stuck, even if the wall-clock timeout hasn't hit.
    let idle_timeout = Duration::from_secs(30);
    let max_prompt_timeout = Duration::from_secs(config.timeout);
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

    // Poll for prompt readiness, handling dialogs as they appear.
    // Track content changes to distinguish "still starting up" from "stuck".
    let prompt_start = std::time::Instant::now();
    let mut last_activity = std::time::Instant::now();
    let mut prev_content = String::new();

    loop {
        let wall_elapsed = prompt_start.elapsed();
        let idle_elapsed = last_activity.elapsed();

        if wall_elapsed >= max_prompt_timeout || idle_elapsed >= idle_timeout {
            let pane = session.capture_pane().unwrap_or_default();
            let tail = content_tail(&pane, 500);
            bail!(
                "[timeout] Timed out waiting for Gemini prompt. Is gemini authenticated? \
                 Try running 'gemini' manually.\nLast captured output:\n{}",
                tail
            );
        }

        let content = session.capture_pane()?;

        // Track activity: reset idle timer when content changes
        if content != prev_content {
            if config.verbose && !prev_content.is_empty() {
                eprintln!("[verbose] Gemini startup activity detected, resetting idle timer");
            }
            last_activity = std::time::Instant::now();
            prev_content = content.clone();
        }

        // Check if the actual prompt is visible
        if gemini_prompt_ready(&content) {
            break;
        }

        // Check for dialogs during startup
        if let Some(kind) = detect_gemini_dialog(&content) {
            if config.verbose {
                eprintln!("[verbose] Dialog detected during prompt wait: {:?}", kind);
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
                        eprintln!("[verbose] Dialog dismissed, continuing...");
                    }
                    last_activity = std::time::Instant::now();
                    prev_content.clear();
                    continue;
                }
            }
        }

        std::thread::sleep(poll_interval);
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

    // Wait for usage data to appear, checking for dialogs
    let pct_re = regex::Regex::new(r"(?i)\d+(?:\.\d+)?%\s*\(Resets?\b")?;
    let data_start = std::time::Instant::now();
    let mut content = String::new();
    let mut data_ready = false;

    while data_start.elapsed() < data_timeout {
        content = session.capture_pane()?;
        if pct_re.is_match(&content) {
            data_ready = true;
            break;
        }

        // Check for dialogs that may have appeared during data wait
        if handle_dialog_check(
            &mut session,
            detect_gemini_dialog,
            "gemini",
            config.approval_policy,
            config.verbose,
        )? {
            // Dialog dismissed, re-send the command
            session.send_keys_literal("/stats session")?;
            std::thread::sleep(Duration::from_millis(500));
            session.send_keys("Enter")?;
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }

        std::thread::sleep(poll_interval);
    }

    if !data_ready {
        let tail = content_tail(&content, 500);
        bail!(
            "[timeout] Timed out waiting for Gemini usage data.\nLast captured output:\n{}",
            tail
        );
    }

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

    std::thread::scope(|s| {
        let claude = s.spawn(|| run_claude(config));
        let codex = s.spawn(|| run_codex(config));
        let gemini = s.spawn(|| run_gemini(config));

        for (name, handle) in [("claude", claude), ("codex", codex), ("gemini", gemini)] {
            match handle.join() {
                Ok(Ok(data)) => results.push(data),
                Ok(Err(e)) => {
                    warnings.insert(name.into(), format!("{:#}", e));
                }
                Err(_) => {
                    warnings.insert(name.into(), "Provider thread panicked".into());
                }
            }
        }
    });

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

    // ── gemini_prompt_ready: legacy path ────────────────────────────

    #[test]
    fn test_gemini_prompt_ready_legacy_gemini_md() {
        assert!(gemini_prompt_ready("Loaded GEMINI.md"));
    }

    #[test]
    fn test_gemini_prompt_ready_legacy_mcp_servers() {
        assert!(gemini_prompt_ready("Found 3 MCP servers"));
    }

    #[test]
    fn test_gemini_prompt_ready_legacy_gemini_prompt() {
        assert!(gemini_prompt_ready("gemini > type here"));
    }

    #[test]
    fn test_gemini_prompt_ready_not_banner_only() {
        // Banner text alone doesn't mean the prompt is ready
        assert!(!gemini_prompt_ready("Welcome to Gemini CLI v0.28.0"));
    }

    #[test]
    fn test_gemini_prompt_ready_not_trust_dialog() {
        // Dialog screens are handled separately, not by prompt readiness
        assert!(!gemini_prompt_ready("Do you trust this folder"));
    }

    #[test]
    fn test_gemini_prompt_ready_legacy_full_startup() {
        assert!(gemini_prompt_ready(
            "Loaded GEMINI.md\nFound 3 MCP servers\ngemini >"
        ));
    }

    // ── gemini_prompt_ready: new path ───────────────────────────────

    #[test]
    fn test_gemini_prompt_ready_bare_gt_entire_line() {
        assert!(gemini_prompt_ready("some header\n>\nmore text"));
    }

    #[test]
    fn test_gemini_prompt_ready_bare_gt_with_space() {
        assert!(gemini_prompt_ready("header\n> \nfooter"));
    }

    #[test]
    fn test_gemini_prompt_ready_gt_with_trailing_content() {
        assert!(gemini_prompt_ready("header\n> type your message"));
    }

    #[test]
    fn test_gemini_prompt_ready_no_false_positive_gt_in_text() {
        assert!(!gemini_prompt_ready("value > 5"));
    }

    #[test]
    fn test_gemini_prompt_ready_no_false_positive_arrow() {
        assert!(!gemini_prompt_ready("use -> arrow"));
    }

    #[test]
    fn test_gemini_prompt_ready_no_false_positive_comparison() {
        assert!(!gemini_prompt_ready("5 > 3"));
    }

    // ── gemini_prompt_ready: startup text must NOT match ───────────
    // These appear before the CLI is ready; handled by dialog detection.

    #[test]
    fn test_gemini_prompt_ready_not_signed_in() {
        assert!(!gemini_prompt_ready("Signed in as user@gmail.com"));
    }

    #[test]
    fn test_gemini_prompt_ready_not_logged_in() {
        assert!(!gemini_prompt_ready("Logged in as user@gmail.com"));
    }

    #[test]
    fn test_gemini_prompt_ready_not_logged_in_with() {
        assert!(!gemini_prompt_ready(
            "Logged in with Google: user@gmail.com"
        ));
    }

    #[test]
    fn test_gemini_prompt_ready_not_model_indicator() {
        assert!(!gemini_prompt_ready("Model: gemini-2.5-pro"));
    }

    #[test]
    fn test_gemini_prompt_ready_not_select_theme() {
        assert!(!gemini_prompt_ready("Select a theme"));
    }

    #[test]
    fn test_gemini_prompt_ready_not_update_available() {
        assert!(!gemini_prompt_ready("Update available: v0.29.0"));
    }

    #[test]
    fn test_gemini_prompt_ready_not_terms() {
        assert!(!gemini_prompt_ready("Please accept the terms of service"));
    }

    #[test]
    fn test_gemini_prompt_ready_what_can_i_help() {
        assert!(gemini_prompt_ready("What can I help you with?"));
    }

    // ── gemini_prompt_ready: case insensitivity ─────────────────────

    #[test]
    fn test_gemini_prompt_ready_lowercase_gemini_md() {
        assert!(gemini_prompt_ready("loaded gemini.md from disk"));
    }

    #[test]
    fn test_gemini_prompt_ready_uppercase_mcp_servers() {
        assert!(gemini_prompt_ready("Found 3 MCP SERVERS configured"));
    }

    // ── gemini_prompt_ready: negative tests ─────────────────────────

    #[test]
    fn test_gemini_prompt_ready_empty() {
        assert!(!gemini_prompt_ready(""));
    }

    #[test]
    fn test_gemini_prompt_ready_loading() {
        assert!(!gemini_prompt_ready("Loading..."));
    }

    #[test]
    fn test_gemini_prompt_ready_processing() {
        assert!(!gemini_prompt_ready("Processing request..."));
    }

    #[test]
    fn test_gemini_prompt_ready_random_text() {
        assert!(!gemini_prompt_ready(
            "The quick brown fox jumps over the lazy dog"
        ));
    }

    // ── gemini_prompt_ready: additional edge cases ──────────────────

    #[test]
    fn test_gemini_prompt_ready_not_google_account() {
        assert!(!gemini_prompt_ready("Using Google Account user@gmail.com"));
    }

    #[test]
    fn test_gemini_prompt_ready_not_color_theme() {
        assert!(!gemini_prompt_ready("Pick a color theme for the CLI"));
    }

    #[test]
    fn test_gemini_prompt_ready_no_false_positive_gt_no_space() {
        // ">foo" without space after > should NOT match
        assert!(!gemini_prompt_ready(">foo"));
    }

    #[test]
    fn test_gemini_prompt_ready_bare_gt_only_content() {
        assert!(gemini_prompt_ready(">"));
    }

    #[test]
    fn test_gemini_prompt_ready_gt_in_multiline_no_match() {
        // > embedded in text across multiple lines, never at line start
        assert!(!gemini_prompt_ready("line1\nvalue > 5\nline3"));
    }

    #[test]
    fn test_gemini_prompt_ready_what_can_i_help_uppercase() {
        assert!(gemini_prompt_ready("WHAT CAN I HELP you with today?"));
    }

    // ── gemini_prompt_ready: data regex ─────────────────────────────

    #[test]
    fn test_gemini_data_regex_case_insensitive() {
        let re = regex::Regex::new(r"(?i)\d+(?:\.\d+)?%\s*\(Resets?\b").unwrap();
        assert!(re.is_match("45.2% (Resets in 3 hours)"));
        assert!(re.is_match("45.2% (resets in 3 hours)"));
        assert!(re.is_match("45.2% (RESETS IN 3 HOURS)"));
        assert!(re.is_match("100% (Reset tomorrow)"));
        assert!(re.is_match("100% (reset tomorrow)"));
    }

    #[test]
    fn test_gemini_data_regex_no_false_positive() {
        let re = regex::Regex::new(r"(?i)\d+(?:\.\d+)?%\s*\(Resets?\b").unwrap();
        assert!(!re.is_match("45% (Resetting)"));
        assert!(!re.is_match("45% used"));
        assert!(!re.is_match("no percentage here"));
    }

    // ── content_tail ────────────────────────────────────────────────

    #[test]
    fn test_content_tail_shorter_than_max() {
        assert_eq!(content_tail("hello", 500), "hello");
    }

    #[test]
    fn test_content_tail_exact_max() {
        assert_eq!(content_tail("abc", 3), "abc");
    }

    #[test]
    fn test_content_tail_longer_than_max() {
        assert_eq!(content_tail("hello world", 5), "world");
    }

    #[test]
    fn test_content_tail_empty() {
        assert_eq!(content_tail("", 500), "");
    }

    #[test]
    fn test_content_tail_unicode() {
        // Ensure char-based truncation doesn't split codepoints
        assert_eq!(content_tail("héllo wörld", 5), "wörld");
    }
}
