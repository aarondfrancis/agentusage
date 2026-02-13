#![deny(warnings)]

mod dialog;
mod parser;
mod tmux;
mod types;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::Duration;

use dialog::{detect_claude_dialog, detect_codex_dialog, detect_gemini_dialog, dialog_error_message, dismiss_dialog};
use parser::{parse_claude_output, parse_codex_output, parse_gemini_output};
use tmux::TmuxSession;
use types::{ApprovalPolicy, DialogKind, PercentKind, UsageData};

#[derive(Parser)]
#[command(name = "agentusage", about = "Check Claude Code, Codex, and Gemini CLI usage limits")]
struct Cli {
    /// Check only Claude Code usage
    #[arg(long)]
    claude: bool,

    /// Check only Codex usage
    #[arg(long)]
    codex: bool,

    /// Check only Gemini CLI usage
    #[arg(long)]
    gemini: bool,

    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// Max time to wait for data in seconds
    #[arg(long, default_value = "45")]
    timeout: u64,

    /// Print debug info (raw captured text, timing)
    #[arg(long)]
    verbose: bool,

    /// How to handle interactive dialogs (trust, update, terms)
    #[arg(long, value_enum, default_value = "fail")]
    approval_policy: ApprovalPolicy,

    /// Kill all stale agentusage tmux sessions and exit
    #[arg(long)]
    cleanup: bool,
}

fn check_command_exists(cmd: &str) -> Result<()> {
    match Command::new(cmd).arg("--version").output() {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("[tool-missing] {} CLI not found. Make sure it is installed and on your PATH.", cmd);
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
    session: &TmuxSession,
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

fn run_claude(cli: &Cli) -> Result<UsageData> {
    check_command_exists("claude")?;

    let session = TmuxSession::new()?;
    let poll_interval = Duration::from_millis(500);
    let prompt_timeout = Duration::from_secs(30);
    let data_timeout = Duration::from_secs(cli.timeout);

    if cli.verbose {
        eprintln!("[verbose] Created tmux session: {}", session.name);
    }

    // Launch claude CLI
    session.send_keys_literal("claude")?;
    session.send_keys("Enter")?;

    if cli.verbose {
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
        cli.verbose,
    );

    if prompt_result.is_err() {
        // Check for dialogs before giving up
        if handle_dialog_check(&session, detect_claude_dialog, "claude", cli.approval_policy, cli.verbose)? {
            // Dialog dismissed, retry waiting for prompt
            session.wait_for(
                |content| {
                    let t = content.trim();
                    t.contains('>') || t.contains('❯') || t.contains("Tips")
                },
                prompt_timeout,
                poll_interval,
                true,
                cli.verbose,
            ).context("[timeout] Timed out waiting for Claude prompt after dismissing dialog.")?;
        } else {
            return Err(prompt_result.unwrap_err().context(
                "Timed out waiting for Claude prompt. Is claude authenticated? Try running 'claude' manually."
            ));
        }
    }

    // Wait for TUI to stabilize instead of fixed sleep
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, cli.verbose);

    if cli.verbose {
        let content = session.capture_pane()?;
        eprintln!("[verbose] Prompt detected. Current pane:\n{}", content);
    }

    // Type /status — triggers autocomplete, then Enter to select and execute
    session.send_keys_literal("/status")?;
    std::thread::sleep(Duration::from_millis(800));

    if cli.verbose {
        let content = session.capture_pane()?;
        eprintln!("[verbose] After typing /status:\n{}", content);
    }

    session.send_keys("Enter")?;

    if cli.verbose {
        eprintln!("[verbose] Sent Enter, waiting for status screen...");
    }

    // Wait for the actual status screen (not the autocomplete dropdown)
    session.wait_for(
        |content| {
            let has_tabs = content.contains("Config") || content.contains("Usage");
            let is_autocomplete = content.contains("/statusline") || content.contains("/stats");
            has_tabs && !is_autocomplete
        },
        Duration::from_secs(15),
        poll_interval,
        false,
        cli.verbose,
    ).context("[timeout] Timed out waiting for status screen")?;

    std::thread::sleep(Duration::from_millis(500));

    // Navigate to Usage tab using Tab key
    for i in 0..5 {
        session.send_keys("Tab")?;
        std::thread::sleep(Duration::from_millis(300));

        let content = session.capture_pane()?;
        if content.contains("% used") || content.contains("Resets") {
            if cli.verbose {
                eprintln!("[verbose] Reached Usage tab after {} Tab presses", i + 1);
            }
            break;
        }
    }

    if cli.verbose {
        eprintln!("[verbose] Navigated tabs, waiting for usage data...");
    }

    let pct_re = regex::Regex::new(r"\d+%\s*used")?;
    let content = session.wait_for(
        |content| pct_re.is_match(content),
        data_timeout,
        poll_interval,
        false,
        cli.verbose,
    ).context("[timeout] Timed out waiting for usage data. Check your internet connection.")?;

    // Wait for TUI to stabilize instead of fixed sleep
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, cli.verbose);

    let final_content = session.capture_pane()?;

    if cli.verbose {
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

fn run_codex(cli: &Cli) -> Result<UsageData> {
    check_command_exists("codex")?;

    let session = TmuxSession::new()?;
    let poll_interval = Duration::from_millis(500);
    let prompt_timeout = Duration::from_secs(30);
    let data_timeout = Duration::from_secs(cli.timeout);

    if cli.verbose {
        eprintln!("[verbose] Created tmux session: {}", session.name);
    }

    // Launch codex CLI
    session.send_keys_literal("codex")?;
    session.send_keys("Enter")?;

    if cli.verbose {
        eprintln!("[verbose] Launched codex, waiting for prompt...");
    }

    // Codex prompt shows "› ..." and "? for shortcuts" at the bottom.
    // Must NOT match ">_" in the Codex banner header which appears early.
    let prompt_result = session.wait_for(
        |content| content.contains("? for shortcuts"),
        prompt_timeout,
        poll_interval,
        false,
        cli.verbose,
    );

    if prompt_result.is_err() {
        // Check for dialogs before giving up
        if handle_dialog_check(&session, detect_codex_dialog, "codex", cli.approval_policy, cli.verbose)? {
            // Dialog dismissed, retry waiting for prompt
            session.wait_for(
                |content| content.contains("? for shortcuts"),
                prompt_timeout,
                poll_interval,
                false,
                cli.verbose,
            ).context("[timeout] Timed out waiting for Codex prompt after dismissing dialog.")?;
        } else {
            return Err(prompt_result.unwrap_err().context(
                "Timed out waiting for Codex prompt. Is codex authenticated? Try running 'codex' manually."
            ));
        }
    }

    // Wait for TUI to stabilize instead of fixed sleep
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, cli.verbose);

    if cli.verbose {
        let content = session.capture_pane()?;
        eprintln!("[verbose] Prompt detected. Current pane:\n{}", content);
    }

    // Codex /status prints inline — no autocomplete, no tabs
    session.send_keys_literal("/status")?;
    std::thread::sleep(Duration::from_millis(500));
    session.send_keys("Enter")?;

    if cli.verbose {
        eprintln!("[verbose] Sent /status + Enter, waiting for usage data...");
    }

    // Wait for limit data to appear
    let limit_re = regex::Regex::new(r"\d+%\s*left")?;
    let content = session.wait_for(
        |content| limit_re.is_match(content),
        data_timeout,
        poll_interval,
        false,
        cli.verbose,
    ).context("[timeout] Timed out waiting for Codex usage data.")?;

    // Wait for all data to render
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, cli.verbose);

    let final_content = session.capture_pane()?;

    if cli.verbose {
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

fn run_gemini(cli: &Cli) -> Result<UsageData> {
    check_command_exists("gemini")?;

    let session = TmuxSession::new()?;
    let poll_interval = Duration::from_millis(500);
    let prompt_timeout = Duration::from_secs(30);
    let data_timeout = Duration::from_secs(cli.timeout);

    if cli.verbose {
        eprintln!("[verbose] Created tmux session: {}", session.name);
    }

    // Launch gemini CLI
    session.send_keys_literal("gemini")?;
    session.send_keys("Enter")?;

    if cli.verbose {
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
        cli.verbose,
    );

    if prompt_result.is_err() {
        // Check for dialogs before giving up
        if handle_dialog_check(&session, detect_gemini_dialog, "gemini", cli.approval_policy, cli.verbose)? {
            // Dialog dismissed, retry waiting for prompt
            session.wait_for(
                |content| {
                    content.contains("GEMINI.md")
                        || content.contains("MCP servers")
                        || content.contains("gemini >")
                        || content.contains("Gemini CLI")
                },
                prompt_timeout,
                poll_interval,
                false,
                cli.verbose,
            ).context("[timeout] Timed out waiting for Gemini prompt after dismissing dialog.")?;
        } else {
            return Err(prompt_result.unwrap_err().context(
                "Timed out waiting for Gemini prompt. Is gemini authenticated? Try running 'gemini' manually."
            ));
        }
    } else {
        // wait_for succeeded — check if what we matched was actually a dialog
        let content = session.capture_pane()?;
        if let Some(kind) = detect_gemini_dialog(&content) {
            if cli.verbose {
                eprintln!("[verbose] Dialog detected after prompt wait: {:?}", kind);
            }
            match cli.approval_policy {
                ApprovalPolicy::Fail => {
                    bail!("[timeout] {}", dialog_error_message(&kind, "gemini"));
                }
                ApprovalPolicy::Accept => {
                    let dismissed = dismiss_dialog(&kind, &session)?;
                    if !dismissed {
                        bail!("[timeout] {}", dialog_error_message(&kind, "gemini"));
                    }
                    if cli.verbose {
                        eprintln!("[verbose] Dialog dismissed, waiting for actual prompt...");
                    }
                    // Re-wait for the actual prompt after dialog dismissal
                    session.wait_for(
                        |content| {
                            content.contains("GEMINI.md")
                                || content.contains("MCP servers")
                                || content.contains("gemini >")
                                || content.contains("Gemini CLI")
                        },
                        prompt_timeout,
                        poll_interval,
                        false,
                        cli.verbose,
                    ).context("[timeout] Timed out waiting for Gemini prompt after dismissing dialog.")?;
                }
            }
        }
    }

    // Wait for TUI to stabilize instead of fixed sleep
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, cli.verbose);

    if cli.verbose {
        let content = session.capture_pane()?;
        eprintln!("[verbose] Prompt detected. Current pane:\n{}", content);
    }

    // Type /stats session — Gemini uses this command, not /status
    session.send_keys_literal("/stats session")?;
    std::thread::sleep(Duration::from_millis(500));
    session.send_keys("Enter")?;

    if cli.verbose {
        eprintln!("[verbose] Sent /stats session + Enter, waiting for usage data...");
    }

    // Wait for usage data to appear
    let pct_re = regex::Regex::new(r"\d+(?:\.\d+)?%\s*\(Resets?")?;
    let content = session.wait_for(
        |content| pct_re.is_match(content),
        data_timeout,
        poll_interval,
        false,
        cli.verbose,
    ).context("[timeout] Timed out waiting for Gemini usage data.")?;

    // Wait for all data to render
    let _ = session.wait_for_stable(Duration::from_secs(2), poll_interval, cli.verbose);

    let final_content = session.capture_pane()?;

    if cli.verbose {
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

struct AllResults {
    results: Vec<UsageData>,
    warnings: BTreeMap<String, String>,
}

fn run_all(cli: &Cli) -> AllResults {
    let mut results = Vec::new();
    let mut warnings = BTreeMap::new();

    match run_claude(cli) {
        Ok(data) => results.push(data),
        Err(e) => { warnings.insert("claude".into(), strip_error_tags(&format!("{:#}", e))); }
    }

    match run_codex(cli) {
        Ok(data) => results.push(data),
        Err(e) => { warnings.insert("codex".into(), strip_error_tags(&format!("{:#}", e))); }
    }

    match run_gemini(cli) {
        Ok(data) => results.push(data),
        Err(e) => { warnings.insert("gemini".into(), strip_error_tags(&format!("{:#}", e))); }
    }

    AllResults { results, warnings }
}

fn print_human(data: &UsageData) {
    let title = match data.provider.as_str() {
        "codex" => "Codex Usage",
        "gemini" => "Gemini Usage",
        _ => "Claude Code Usage",
    };
    println!("{}", title);
    println!("{}", "─".repeat(60));

    for entry in &data.entries {
        let kind = match entry.percent_kind {
            PercentKind::Used => "used",
            PercentKind::Left => "left",
        };

        let spent_str = entry
            .spent
            .as_ref()
            .map(|s| format!(" · {}", s))
            .unwrap_or_default();

        let requests_str = entry
            .requests
            .as_ref()
            .map(|r| format!(" · {} reqs", r))
            .unwrap_or_default();

        let reset_str = if entry.reset_info.is_empty() {
            String::new()
        } else {
            format!(" · {}", entry.reset_info)
        };

        println!(
            "{:<30} {:>5}% {}{}{}{}",
            format!("{}:", entry.label),
            entry.percent,
            kind,
            requests_str,
            spent_str,
            reset_str,
        );
    }
}

fn print_human_multi(results: &[UsageData]) {
    for (i, data) in results.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_human(data);
    }
}

fn print_json(data: &UsageData) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(data)?);
    Ok(())
}

fn print_json_multi(all: &AllResults) -> Result<()> {
    let mut wrapper = serde_json::json!({ "results": all.results });
    if !all.warnings.is_empty() {
        wrapper["warnings"] = serde_json::json!(all.warnings);
    }
    println!("{}", serde_json::to_string_pretty(&wrapper)?);
    Ok(())
}

/// Determine exit code from error message tags.
fn exit_code_from_error(err: &str) -> i32 {
    if err.contains("[tool-missing]") {
        2
    } else if err.contains("[timeout]") {
        3
    } else if err.contains("[parse-failure]") {
        4
    } else {
        1
    }
}

/// Strip internal error tags from user-facing message.
fn strip_error_tags(msg: &str) -> String {
    msg.replace("[tool-missing] ", "")
        .replace("[timeout] ", "")
        .replace("[parse-failure] ", "")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── exit_code_from_error ────────────────────────────────────────

    #[test]
    fn test_exit_code_tool_missing() {
        assert_eq!(exit_code_from_error("[tool-missing] claude CLI not found"), 2);
    }

    #[test]
    fn test_exit_code_timeout() {
        assert_eq!(exit_code_from_error("[timeout] Timed out after 45s"), 3);
    }

    #[test]
    fn test_exit_code_parse_failure() {
        assert_eq!(exit_code_from_error("[parse-failure] No usage data found"), 4);
    }

    #[test]
    fn test_exit_code_general() {
        assert_eq!(exit_code_from_error("something else went wrong"), 1);
    }

    #[test]
    fn test_exit_code_empty_string() {
        assert_eq!(exit_code_from_error(""), 1);
    }

    #[test]
    fn test_exit_code_tag_embedded_in_context() {
        // anyhow context wrapping: "outer: [timeout] inner"
        assert_eq!(exit_code_from_error("Timed out waiting for prompt: [timeout] Timed out after 30s"), 3);
    }

    // ── strip_error_tags ────────────────────────────────────────────

    #[test]
    fn test_strip_tool_missing_tag() {
        assert_eq!(
            strip_error_tags("[tool-missing] claude CLI not found"),
            "claude CLI not found"
        );
    }

    #[test]
    fn test_strip_timeout_tag() {
        assert_eq!(
            strip_error_tags("[timeout] Timed out after 45s"),
            "Timed out after 45s"
        );
    }

    #[test]
    fn test_strip_parse_failure_tag() {
        assert_eq!(
            strip_error_tags("[parse-failure] No usage data found"),
            "No usage data found"
        );
    }

    #[test]
    fn test_strip_no_tags() {
        assert_eq!(strip_error_tags("plain error"), "plain error");
    }

    #[test]
    fn test_strip_multiple_tags_in_chained_error() {
        // anyhow can chain errors: "context: [timeout] inner message"
        let msg = "Waiting failed: [timeout] Timed out after 30s";
        let stripped = strip_error_tags(msg);
        assert_eq!(stripped, "Waiting failed: Timed out after 30s");
    }

    // ── pick_richer ─────────────────────────────────────────────────

    #[test]
    fn test_pick_richer_first_has_more() {
        let a = UsageData {
            provider: "claude".into(),
            entries: vec![
                types::UsageEntry {
                    label: "session".into(),
                    percent: 5.0,
                    percent_kind: PercentKind::Used,
                    reset_info: "Resets 2pm".into(),
                    spent: None,
                    requests: None,
                },
                types::UsageEntry {
                    label: "week".into(),
                    percent: 10.0,
                    percent_kind: PercentKind::Used,
                    reset_info: "Resets Feb 20".into(),
                    spent: None,
                    requests: None,
                },
            ],
        };
        let b = UsageData {
            provider: "claude".into(),
            entries: vec![types::UsageEntry {
                label: "session".into(),
                percent: 5.0,
                percent_kind: PercentKind::Used,
                reset_info: "Resets 2pm".into(),
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
            entries: vec![types::UsageEntry {
                label: "session".into(),
                percent: 5.0,
                percent_kind: PercentKind::Used,
                reset_info: "Resets 2pm".into(),
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
            entries: vec![types::UsageEntry {
                label: "from_a".into(),
                percent: 5.0,
                percent_kind: PercentKind::Used,
                reset_info: String::new(),
                spent: None,
                requests: None,
            }],
        };
        let b = UsageData {
            provider: "claude".into(),
            entries: vec![types::UsageEntry {
                label: "from_b".into(),
                percent: 10.0,
                percent_kind: PercentKind::Used,
                reset_info: String::new(),
                spent: None,
                requests: None,
            }],
        };
        let result = pick_richer(a, b);
        assert_eq!(result.entries[0].label, "from_a");
    }

    #[test]
    fn test_pick_richer_both_empty() {
        let a = UsageData { provider: "claude".into(), entries: vec![] };
        let b = UsageData { provider: "claude".into(), entries: vec![] };
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

    // ── CLI flag parsing ──────────────────────────────────────────

    #[test]
    fn test_cli_default_no_flags() {
        let cli = Cli::try_parse_from(["agentusage"]).unwrap();
        assert!(!cli.claude);
        assert!(!cli.codex);
        assert!(!cli.gemini);
    }

    #[test]
    fn test_cli_claude_flag() {
        let cli = Cli::try_parse_from(["agentusage", "--claude"]).unwrap();
        assert!(cli.claude);
        assert!(!cli.codex);
        assert!(!cli.gemini);
    }

    #[test]
    fn test_cli_codex_flag() {
        let cli = Cli::try_parse_from(["agentusage", "--codex"]).unwrap();
        assert!(!cli.claude);
        assert!(cli.codex);
    }

    #[test]
    fn test_cli_gemini_flag() {
        let cli = Cli::try_parse_from(["agentusage", "--gemini"]).unwrap();
        assert!(!cli.claude);
        assert!(cli.gemini);
    }

    #[test]
    fn test_cli_flags_no_conflict() {
        // Multiple provider flags should parse without error (even if main() only uses first match)
        let cli = Cli::try_parse_from(["agentusage", "--claude", "--codex"]).unwrap();
        assert!(cli.claude);
        assert!(cli.codex);
    }

    #[test]
    fn test_cli_json_with_provider() {
        let cli = Cli::try_parse_from(["agentusage", "--claude", "--json"]).unwrap();
        assert!(cli.claude);
        assert!(cli.json);
    }

    // ── JSON multi output ─────────────────────────────────────────

    fn sample_usage(provider: &str) -> UsageData {
        UsageData {
            provider: provider.into(),
            entries: vec![types::UsageEntry {
                label: "session".into(),
                percent: 42.0,
                percent_kind: PercentKind::Used,
                reset_info: "Resets 2pm".into(),
                spent: None,
                requests: None,
            }],
        }
    }

    #[test]
    fn test_json_multi_structure_no_warnings() {
        let all = AllResults {
            results: vec![sample_usage("claude")],
            warnings: BTreeMap::new(),
        };
        let mut wrapper = serde_json::json!({ "results": all.results });
        if !all.warnings.is_empty() {
            wrapper["warnings"] = serde_json::json!(all.warnings);
        }
        assert!(wrapper.get("results").unwrap().is_array());
        assert!(wrapper.get("warnings").is_none());
    }

    #[test]
    fn test_json_multi_structure_with_warnings() {
        let mut warnings = BTreeMap::new();
        warnings.insert("codex".to_string(), "tool not found".to_string());
        let all = AllResults {
            results: vec![sample_usage("claude")],
            warnings,
        };
        let mut wrapper = serde_json::json!({ "results": all.results });
        if !all.warnings.is_empty() {
            wrapper["warnings"] = serde_json::json!(all.warnings);
        }
        assert!(wrapper.get("results").unwrap().is_array());
        let warnings = wrapper.get("warnings").unwrap().as_object().unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings.contains_key("codex"));
        assert_eq!(warnings["codex"], "tool not found");
    }

    #[test]
    fn test_json_multi_multiple_results() {
        let mut warnings = BTreeMap::new();
        warnings.insert("codex".to_string(), "tool not found".to_string());
        let all = AllResults {
            results: vec![sample_usage("claude"), sample_usage("gemini")],
            warnings,
        };
        let wrapper = serde_json::json!({ "results": all.results, "warnings": all.warnings });
        let results = wrapper["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["provider"], "claude");
        assert_eq!(results[1]["provider"], "gemini");
        assert_eq!(wrapper["warnings"]["codex"], "tool not found");
    }

    #[test]
    fn test_json_multi_all_failed() {
        let mut warnings = BTreeMap::new();
        warnings.insert("claude".to_string(), "tool not found".to_string());
        warnings.insert("codex".to_string(), "tool not found".to_string());
        warnings.insert("gemini".to_string(), "tool not found".to_string());
        let all = AllResults {
            results: vec![],
            warnings,
        };
        assert!(all.results.is_empty());
        assert_eq!(all.warnings.len(), 3);
    }
}

fn main() {
    let cli = Cli::parse();

    // Handle --cleanup
    if cli.cleanup {
        TmuxSession::kill_all_stale_sessions();
        return;
    }

    // Set up Ctrl+C handler
    ctrlc::set_handler(|| {
        tmux::SHUTDOWN.store(true, Ordering::SeqCst);
        // Best-effort: kill the entire agentusage tmux server
        let _ = Command::new("tmux")
            .args(["-L", "agentusage", "kill-server"])
            .status();
        std::process::exit(130);
    })
    .expect("Failed to set Ctrl+C handler");

    if cli.claude || cli.codex || cli.gemini {
        // Single provider mode
        let result = if cli.claude {
            run_claude(&cli)
        } else if cli.codex {
            run_codex(&cli)
        } else {
            run_gemini(&cli)
        };

        match result {
            Ok(data) => {
                if cli.json {
                    if let Err(e) = print_json(&data) {
                        eprintln!("Error formatting JSON: {}", e);
                        std::process::exit(1);
                    }
                } else {
                    print_human(&data);
                }
            }
            Err(e) => {
                let msg = format!("{:#}", e);
                let code = exit_code_from_error(&msg);
                eprintln!("Error: {}", strip_error_tags(&msg));
                std::process::exit(code);
            }
        }
    } else {
        // All providers mode
        let all = run_all(&cli);

        if all.results.is_empty() {
            if cli.json {
                let wrapper = serde_json::json!({
                    "results": [],
                    "warnings": all.warnings,
                    "error": "All providers failed.",
                });
                println!("{}", serde_json::to_string_pretty(&wrapper).unwrap());
            } else {
                for (provider, msg) in &all.warnings {
                    eprintln!("Warning ({}): {}", provider, msg);
                }
                eprintln!("Error: All providers failed.");
            }
            std::process::exit(1);
        }

        if cli.json {
            if let Err(e) = print_json_multi(&all) {
                eprintln!("Error formatting JSON: {}", e);
                std::process::exit(1);
            }
        } else {
            for (provider, msg) in &all.warnings {
                eprintln!("Warning ({}): {}", provider, msg);
            }
            print_human_multi(&all.results);
        }
    }
}
