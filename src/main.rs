mod parser;
mod tmux;
mod types;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::process::Command;
use std::time::Duration;

use parser::{parse_claude_output, parse_codex_output, parse_gemini_output};
use tmux::TmuxSession;
use types::{PercentKind, UsageData};

#[derive(Parser)]
#[command(name = "agentusage", about = "Check Claude Code, Codex, and Gemini CLI usage limits")]
struct Cli {
    /// Check Codex usage instead of Claude Code
    #[arg(long, conflicts_with = "gemini")]
    codex: bool,

    /// Check Gemini CLI usage instead of Claude Code
    #[arg(long, conflicts_with = "codex")]
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
}

fn check_command_exists(cmd: &str) -> Result<()> {
    Command::new("which")
        .arg(cmd)
        .output()
        .context(format!("Failed to check for {}", cmd))?
        .status
        .success()
        .then_some(())
        .context(format!("{} CLI not found. Make sure it is installed and on your PATH.", cmd))
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

    session.wait_for(
        |content| {
            let t = content.trim();
            t.contains('>') || t.contains('❯') || t.contains("Tips")
        },
        prompt_timeout,
        poll_interval,
        true,
        cli.verbose,
    ).context("Timed out waiting for Claude prompt. Is claude authenticated? Try running 'claude' manually.")?;

    // Extra delay to make sure TUI is fully ready for input
    std::thread::sleep(Duration::from_secs(1));

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
    ).context("Timed out waiting for status screen")?;

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
    ).context("Timed out waiting for usage data. Check your internet connection.")?;

    std::thread::sleep(Duration::from_secs(1));

    let final_content = session.capture_pane()?;

    if cli.verbose {
        eprintln!("[verbose] Raw captured text:\n{}", final_content);
    }

    let data = parse_claude_output(&final_content)?;

    if data.entries.is_empty() {
        let data = parse_claude_output(&content)?;
        if data.entries.is_empty() {
            bail!("No usage data found in captured output. Run with --verbose to see raw text.");
        }
        return Ok(data);
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
    session.wait_for(
        |content| content.contains("? for shortcuts"),
        prompt_timeout,
        poll_interval,
        false,
        cli.verbose,
    ).context("Timed out waiting for Codex prompt. Is codex authenticated? Try running 'codex' manually.")?;

    std::thread::sleep(Duration::from_secs(1));

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
    ).context("Timed out waiting for Codex usage data.")?;

    // Wait a bit for all data to render
    std::thread::sleep(Duration::from_secs(1));

    let final_content = session.capture_pane()?;

    if cli.verbose {
        eprintln!("[verbose] Raw captured text:\n{}", final_content);
    }

    let data = parse_codex_output(&final_content)?;

    if data.entries.is_empty() {
        let data = parse_codex_output(&content)?;
        if data.entries.is_empty() {
            bail!("No usage data found in captured output. Run with --verbose to see raw text.");
        }
        return Ok(data);
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

    // Wait for Gemini prompt — indicated by GEMINI.md or MCP servers text
    session.wait_for(
        |content| content.contains("GEMINI.md") || content.contains("MCP servers"),
        prompt_timeout,
        poll_interval,
        false,
        cli.verbose,
    ).context("Timed out waiting for Gemini prompt. Is gemini authenticated? Try running 'gemini' manually.")?;

    // Check if trust dialog appeared
    let content = session.capture_pane()?;
    if content.contains("Do you trust this folder") {
        if cli.verbose {
            eprintln!("[verbose] Trust dialog detected, pressing Enter to accept...");
        }
        session.send_keys("Enter")?;
        std::thread::sleep(Duration::from_secs(1));

        // Re-wait for the actual prompt after trust dialog
        session.wait_for(
            |content| content.contains("GEMINI.md") || content.contains("MCP servers"),
            prompt_timeout,
            poll_interval,
            false,
            cli.verbose,
        ).context("Timed out waiting for Gemini prompt after trust dialog.")?;
    }

    std::thread::sleep(Duration::from_secs(1));

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
    ).context("Timed out waiting for Gemini usage data.")?;

    // Wait for all data to render
    std::thread::sleep(Duration::from_secs(1));

    let final_content = session.capture_pane()?;

    if cli.verbose {
        eprintln!("[verbose] Raw captured text:\n{}", final_content);
    }

    let data = parse_gemini_output(&final_content)?;

    if data.entries.is_empty() {
        let data = parse_gemini_output(&content)?;
        if data.entries.is_empty() {
            bail!("No usage data found in captured output. Run with --verbose to see raw text.");
        }
        return Ok(data);
    }

    Ok(data)
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

fn print_json(data: &UsageData) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(data)?);
    Ok(())
}

fn main() {
    let cli = Cli::parse();

    let result = if cli.codex {
        run_codex(&cli)
    } else if cli.gemini {
        run_gemini(&cli)
    } else {
        run_claude(&cli)
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
            eprintln!("Error: {:#}", e);
            std::process::exit(1);
        }
    }
}
