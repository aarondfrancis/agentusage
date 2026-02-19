#![deny(warnings)]

use anyhow::Result;
use clap::Parser;
use comfy_table::{presets::ASCII_BORDERS_ONLY_CONDENSED, Table};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentusage::{
    run_all, run_claude, run_codex, run_gemini, AllResults, ApprovalPolicy, PercentKind, UsageConfig,
    UsageData, UsageEntry,
};

#[derive(Parser)]
#[command(
    name = "agentusage",
    version,
    about = "Check Claude Code, Codex, and Gemini CLI usage limits",
    long_about = "Check Claude Code, Codex, and Gemini CLI usage limits.\n\n\
        Launches each CLI tool in an isolated pseudo-terminal (openpty), then\n\
        runs its usage/status command,\n\
        parses the output, and reports usage percentages, reset times, and spend.\n\n\
        By default, checks all installed providers. Use --claude, --codex, or\n\
        --gemini to check a single provider.",
    after_help = "\
Examples:
  agentusage                  Check all installed providers
  agentusage --claude         Check only Claude Code
  agentusage --json           Output as machine-readable JSON
  agentusage --claude --json  Single provider, JSON output
  agentusage --timeout 60     Wait up to 60s for data
  agentusage -C ~/project     Run CLI sessions in ~/project
  agentusage --cleanup        Kill tracked PTY child sessions and exit

Exit codes:
  0  Success
  1  General error
  2  Required tool not found (provider CLI)
  3  Timeout waiting for provider output
  4  Failed to parse provider output"
)]
struct Cli {
    /// Check only Claude Code usage
    #[arg(long, help_heading = "Providers", conflicts_with_all = ["codex", "gemini"])]
    claude: bool,

    /// Check only Codex usage
    #[arg(long, help_heading = "Providers", conflicts_with_all = ["claude", "gemini"])]
    codex: bool,

    /// Check only Gemini CLI usage
    #[arg(long, help_heading = "Providers", conflicts_with_all = ["claude", "codex"])]
    gemini: bool,

    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// Max seconds to wait for data [default: 45]
    #[arg(long, default_value = "45", hide_default_value = true)]
    timeout: u64,

    /// Print debug info (raw captured text, timing)
    #[arg(long)]
    verbose: bool,

    /// How to handle interactive dialogs (trust, update, terms) [default: fail]
    #[arg(long, value_enum, default_value = "fail", hide_default_value = true)]
    approval_policy: ApprovalPolicy,

    /// Working directory for the CLI sessions
    #[arg(long, short = 'C')]
    directory: Option<String>,

    /// Kill tracked agentusage PTY child sessions and exit
    #[arg(long)]
    cleanup: bool,

    /// Check if provider CLIs are installed
    #[arg(long)]
    doctor: bool,
}

impl Cli {
    fn to_config(&self) -> UsageConfig {
        UsageConfig {
            timeout: self.timeout,
            verbose: self.verbose,
            approval_policy: self.approval_policy,
            directory: self.directory.clone(),
        }
    }
}

fn run_doctor() {
    let mut all_ok = true;

    // Check providers
    for (cmd, name) in [
        ("claude", "Claude Code"),
        ("codex", "Codex"),
        ("gemini", "Gemini CLI"),
    ] {
        match Command::new(cmd).arg("--version").output() {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout);
                println!("  {}: {}", name, version.trim());
            }
            Ok(_) => {
                println!("  {}: installed (unknown version)", name);
            }
            Err(_) => {
                println!("  {}: not found", name);
                all_ok = false;
            }
        }
    }

    if all_ok {
        println!("\nAll required provider dependencies found.");
    } else {
        println!("\nSome required provider dependencies are missing.");
        std::process::exit(1);
    }
}

struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    fn start(message: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let msg = message.to_string();

        let handle = std::thread::spawn(move || {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut i = 0;
            let mut stderr = std::io::stderr();
            while !stop_clone.load(Ordering::Relaxed) {
                let _ = write!(stderr, "\r{} {}", frames[i % frames.len()], msg);
                let _ = stderr.flush();
                std::thread::sleep(Duration::from_millis(80));
                i += 1;
            }
            // Clear the spinner line
            let _ = write!(stderr, "\r{}\r", " ".repeat(msg.len() + 4));
            let _ = stderr.flush();
        });

        Spinner {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}

// ── Multi-provider progress display ──────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum ProviderStatus {
    Waiting,
    Done,
    Failed,
}

struct MultiSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MultiSpinner {
    fn start(names: &[&str], states: Arc<Mutex<Vec<ProviderStatus>>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();

        let handle = std::thread::spawn(move || {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut i = 0;
            let n = names.len();
            let mut stderr = std::io::stderr();
            let mut first = true;

            while !stop_clone.load(Ordering::Relaxed) {
                if !first && n > 1 {
                    // Move cursor up to overwrite previous lines
                    // Cursor is on line n, move up n-1 to reach line 1
                    let _ = write!(stderr, "\x1b[{}A", n - 1);
                }

                let st = states.lock().unwrap();
                for (j, name) in names.iter().enumerate() {
                    let _ = write!(stderr, "\r\x1b[2K");
                    match st[j] {
                        ProviderStatus::Waiting => {
                            let _ =
                                write!(stderr, "{} Checking {}...", frames[i % frames.len()], name);
                        }
                        ProviderStatus::Done => {
                            let _ = write!(stderr, "\x1b[32m✓\x1b[0m {}", name);
                        }
                        ProviderStatus::Failed => {
                            let _ = write!(stderr, "\x1b[33m✗\x1b[0m {}", name);
                        }
                    }
                    if j < n - 1 {
                        let _ = writeln!(stderr);
                    }
                }
                drop(st);

                // Park cursor on the last line (no trailing newline)
                let _ = stderr.flush();
                first = false;
                std::thread::sleep(Duration::from_millis(80));
                i += 1;
            }

            // Clear all lines
            if !first {
                // Move to first line
                if n > 1 {
                    let _ = write!(stderr, "\x1b[{}A", n - 1);
                }
                for j in 0..n {
                    let _ = write!(stderr, "\r\x1b[2K");
                    if j < n - 1 {
                        let _ = write!(stderr, "\x1b[B");
                    }
                }
                // Return to first line
                if n > 1 {
                    let _ = write!(stderr, "\x1b[{}A", n - 1);
                }
                let _ = write!(stderr, "\r");
                let _ = stderr.flush();
            }
        });

        MultiSpinner {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for MultiSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}

/// Run all providers in parallel with per-provider progress display.
fn run_all_with_progress(config: &UsageConfig) -> AllResults {
    let names = ["claude", "codex", "gemini"];
    let states = Arc::new(Mutex::new(vec![ProviderStatus::Waiting; 3]));
    let spinner = MultiSpinner::start(&names, states.clone());

    let mut results = Vec::new();
    let mut warnings = BTreeMap::new();

    std::thread::scope(|s| {
        let st0 = states.clone();
        let h0 = s.spawn(move || {
            let r = run_claude(config);
            st0.lock().unwrap()[0] = if r.is_ok() {
                ProviderStatus::Done
            } else {
                ProviderStatus::Failed
            };
            r
        });

        let st1 = states.clone();
        let h1 = s.spawn(move || {
            let r = run_codex(config);
            st1.lock().unwrap()[1] = if r.is_ok() {
                ProviderStatus::Done
            } else {
                ProviderStatus::Failed
            };
            r
        });

        let st2 = states.clone();
        let h2 = s.spawn(move || {
            let r = run_gemini(config);
            st2.lock().unwrap()[2] = if r.is_ok() {
                ProviderStatus::Done
            } else {
                ProviderStatus::Failed
            };
            r
        });

        for (name, handle) in [("claude", h0), ("codex", h1), ("gemini", h2)] {
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

    drop(spinner);

    AllResults { results, warnings }
}

fn print_human(data: &UsageData) {
    let title = match data.provider.as_str() {
        "codex" => "Codex Usage",
        "gemini" => "Gemini Usage",
        _ => "Claude Code Usage",
    };
    println!("{}", title);
    let mut table = Table::new();
    table.load_preset(ASCII_BORDERS_ONLY_CONDENSED);
    table.set_header(vec!["Limit", "Remaining", "Days", "Minutes", "Hours", "Spend"]);

    for entry in &data.entries {
        table.add_row(vec![
            entry.label.clone(),
            remaining_pct_cell(entry),
            reset_days_cell(entry),
            reset_minutes_cell(entry),
            reset_hours_cell(entry),
            spent_cell(entry),
        ]);
    }

    println!("{}", table);
}

fn print_human_multi(results: &[UsageData]) {
    let mut table = Table::new();
    table.load_preset(ASCII_BORDERS_ONLY_CONDENSED);
    table.set_header(vec![
        "Provider",
        "Limit",
        "Remaining",
        "Days",
        "Minutes",
        "Hours",
        "Spend",
    ]);

    let mut boundaries = Vec::new();
    let mut row_count = 0usize;
    for (idx, data) in results.iter().enumerate() {
        let mut added_for_provider = 0usize;
        for entry in &data.entries {
            table.add_row(vec![
                provider_label(&data.provider).to_string(),
                entry.label.clone(),
                remaining_pct_cell(entry),
                reset_days_cell(entry),
                reset_minutes_cell(entry),
                reset_hours_cell(entry),
                spent_cell(entry),
            ]);
            row_count += 1;
            added_for_provider += 1;
        }

        if idx + 1 < results.len() && added_for_provider > 0 {
            boundaries.push(row_count);
        }
    }

    let mut lines: Vec<String> = table.to_string().lines().map(|s| s.to_string()).collect();
    if lines.len() >= 4 {
        let divider = lines[0].clone();
        let mut inserted = 0usize;
        for boundary in boundaries {
            let insert_at = 3 + boundary + inserted;
            if insert_at < lines.len().saturating_sub(1) {
                lines.insert(insert_at, divider.clone());
                inserted += 1;
            }
        }
    }

    println!("Usage");
    println!("{}", lines.join("\n"));
}

fn provider_label(provider: &str) -> &str {
    match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        "gemini" => "Gemini",
        _ => provider,
    }
}

fn remaining_pct_cell(entry: &UsageEntry) -> String {
    let remaining = match entry.percent_kind {
        PercentKind::Used => entry.percent_remaining,
        PercentKind::Left => entry.percent_remaining,
    };
    format!("{}%", remaining)
}

fn spent_cell(entry: &UsageEntry) -> String {
    entry.spent.clone().unwrap_or_default()
}

fn reset_days_cell(entry: &UsageEntry) -> String {
    entry
        .reset_minutes
        .map(|mins| format!("{:.2}", mins as f64 / (24.0 * 60.0)))
        .unwrap_or_default()
}

fn reset_minutes_cell(entry: &UsageEntry) -> String {
    entry
        .reset_minutes
        .map(|mins| mins.to_string())
        .unwrap_or_default()
}

fn reset_hours_cell(entry: &UsageEntry) -> String {
    entry
        .reset_minutes
        .map(|mins| format!("{:.2}", mins as f64 / 60.0))
        .unwrap_or_default()
}

/// Build a JSON object for a single provider: { label: { ...fields }, ... }
fn build_provider_json(data: &UsageData) -> serde_json::Value {
    fn round2(v: f64) -> f64 {
        (v * 100.0).round() / 100.0
    }

    let mut entries = serde_json::Map::new();
    for entry in &data.entries {
        let mut obj = serde_json::Map::new();
        obj.insert("percent_used".into(), serde_json::json!(entry.percent_used));
        obj.insert(
            "percent_remaining".into(),
            serde_json::json!(entry.percent_remaining),
        );
        obj.insert("reset_info".into(), serde_json::json!(entry.reset_info));
        if let Some(mins) = entry.reset_minutes {
            obj.insert("reset_minutes".into(), serde_json::json!(mins));
            obj.insert("reset_hours".into(), serde_json::json!(round2(mins as f64 / 60.0)));
            obj.insert(
                "reset_days".into(),
                serde_json::json!(round2(mins as f64 / (24.0 * 60.0))),
            );
        }
        if let Some(ref spent) = entry.spent {
            obj.insert("spent".into(), serde_json::json!(spent));
        }
        if let Some(ref requests) = entry.requests {
            obj.insert("requests".into(), serde_json::json!(requests));
        }
        entries.insert(entry.label.clone(), serde_json::Value::Object(obj));
    }
    serde_json::Value::Object(entries)
}

fn print_json(data: &UsageData) -> Result<()> {
    let mut results = serde_json::Map::new();
    results.insert(data.provider.clone(), build_provider_json(data));

    let wrapper = serde_json::json!({
        "success": true,
        "results": serde_json::Value::Object(results),
    });
    println!("{}", serde_json::to_string_pretty(&wrapper)?);
    Ok(())
}

fn print_json_multi(all: &AllResults) -> Result<()> {
    let mut results = serde_json::Map::new();
    for data in &all.results {
        results.insert(data.provider.clone(), build_provider_json(data));
    }

    // Strip internal tags from warnings for user-facing JSON output
    let stripped_warnings: BTreeMap<String, String> = all
        .warnings
        .iter()
        .map(|(k, v)| (k.clone(), strip_error_tags(v)))
        .collect();

    let mut wrapper = serde_json::json!({
        "success": true,
        "results": serde_json::Value::Object(results),
    });
    if !stripped_warnings.is_empty() {
        wrapper["warnings"] = serde_json::json!(stripped_warnings);
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

fn main() {
    let cli = Cli::parse();

    // Handle --cleanup
    if cli.cleanup {
        agentusage::session::Session::kill_all_stale_sessions();
        return;
    }

    // Handle --doctor
    if cli.doctor {
        run_doctor();
        return;
    }

    agentusage::pty::clear_shutdown();

    // Set up Ctrl+C handler
    ctrlc::set_handler(|| {
        agentusage::pty::request_shutdown();
        agentusage::session::Session::kill_registered_sessions();
        std::process::exit(130);
    })
    .expect("Failed to set Ctrl+C handler");

    let config = cli.to_config();
    let show_progress = !cli.json && !cli.verbose;

    if cli.claude || cli.codex || cli.gemini {
        // Single provider mode
        let provider_name = if cli.claude {
            "claude"
        } else if cli.codex {
            "codex"
        } else {
            "gemini"
        };
        let spinner =
            show_progress.then(|| Spinner::start(&format!("Checking {}...", provider_name)));

        let result = if cli.claude {
            run_claude(&config)
        } else if cli.codex {
            run_codex(&config)
        } else {
            run_gemini(&config)
        };

        drop(spinner);

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
                if cli.json {
                    let wrapper = serde_json::json!({
                        "success": false,
                        "error": strip_error_tags(&msg),
                    });
                    println!("{}", serde_json::to_string_pretty(&wrapper).unwrap());
                } else {
                    eprintln!("Error: {}", strip_error_tags(&msg));
                }
                std::process::exit(code);
            }
        }
    } else {
        // All providers mode (parallel)
        let all = if show_progress {
            run_all_with_progress(&config)
        } else {
            run_all(&config)
        };

        if all.results.is_empty() {
            if cli.json {
                let stripped_warnings: BTreeMap<String, String> = all
                    .warnings
                    .iter()
                    .map(|(k, v)| (k.clone(), strip_error_tags(v)))
                    .collect();
                let wrapper = serde_json::json!({
                    "success": false,
                    "results": {},
                    "warnings": stripped_warnings,
                    "error": "All providers failed.",
                });
                println!("{}", serde_json::to_string_pretty(&wrapper).unwrap());
            } else {
                for (provider, msg) in &all.warnings {
                    eprintln!("Warning ({}): {}", provider, strip_error_tags(msg));
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
                eprintln!("Warning ({}): {}", provider, strip_error_tags(msg));
            }
            print_human_multi(&all.results);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentusage::UsageEntry;

    // ── exit_code_from_error ────────────────────────────────────────

    #[test]
    fn test_exit_code_tool_missing() {
        assert_eq!(
            exit_code_from_error("[tool-missing] claude CLI not found"),
            2
        );
    }

    #[test]
    fn test_exit_code_timeout() {
        assert_eq!(exit_code_from_error("[timeout] Timed out after 45s"), 3);
    }

    #[test]
    fn test_exit_code_parse_failure() {
        assert_eq!(
            exit_code_from_error("[parse-failure] No usage data found"),
            4
        );
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
        assert_eq!(
            exit_code_from_error("Timed out waiting for prompt: [timeout] Timed out after 30s"),
            3
        );
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
    fn test_cli_conflicting_provider_flags_error() {
        // Multiple provider flags should produce a clap error
        assert!(Cli::try_parse_from(["agentusage", "--claude", "--codex"]).is_err());
        assert!(Cli::try_parse_from(["agentusage", "--claude", "--gemini"]).is_err());
        assert!(Cli::try_parse_from(["agentusage", "--codex", "--gemini"]).is_err());
        assert!(Cli::try_parse_from(["agentusage", "--claude", "--codex", "--gemini"]).is_err());
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
            entries: vec![UsageEntry {
                label: "session".into(),
                percent_used: 42,
                percent_kind: PercentKind::Used,
                reset_info: "Resets 2pm".into(),
                percent_remaining: 58,
                reset_minutes: None,
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
        let mut results = serde_json::Map::new();
        for data in &all.results {
            results.insert(data.provider.clone(), build_provider_json(data));
        }
        let mut wrapper = serde_json::json!({
            "success": true,
            "results": serde_json::Value::Object(results),
        });
        if !all.warnings.is_empty() {
            wrapper["warnings"] = serde_json::json!(all.warnings);
        }
        assert_eq!(wrapper.get("success").unwrap(), true);
        assert!(wrapper.get("results").unwrap().is_object());
        assert!(wrapper["results"].get("claude").is_some());
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
        let mut results = serde_json::Map::new();
        for data in &all.results {
            results.insert(data.provider.clone(), build_provider_json(data));
        }
        let mut wrapper = serde_json::json!({
            "success": true,
            "results": serde_json::Value::Object(results),
        });
        if !all.warnings.is_empty() {
            wrapper["warnings"] = serde_json::json!(all.warnings);
        }
        assert_eq!(wrapper.get("success").unwrap(), true);
        assert!(wrapper["results"].get("claude").is_some());
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
        let mut results = serde_json::Map::new();
        for data in &all.results {
            results.insert(data.provider.clone(), build_provider_json(data));
        }
        let wrapper = serde_json::json!({
            "results": serde_json::Value::Object(results),
            "warnings": all.warnings,
        });
        let results = wrapper["results"].as_object().unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.contains_key("claude"));
        assert!(results.contains_key("gemini"));
        // Each provider has a "session" label entry
        assert!(wrapper["results"]["claude"]["session"].is_object());
        assert_eq!(wrapper["results"]["claude"]["session"]["percent_used"], 42);
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

    #[test]
    fn test_build_provider_json_structure() {
        let data = sample_usage("claude");
        let json = build_provider_json(&data);
        let obj = json.as_object().unwrap();
        // Key is the label
        assert!(obj.contains_key("session"));
        let entry = obj["session"].as_object().unwrap();
        assert_eq!(entry["percent_used"], 42);
        assert!(!entry.contains_key("percent_kind"));
        assert_eq!(entry["percent_remaining"], 58);
        // reset_minutes is None, should be absent
        assert!(!entry.contains_key("reset_minutes"));
        assert!(!entry.contains_key("reset_hours"));
        assert!(!entry.contains_key("reset_days"));
        // spent is None, should be absent
        assert!(!entry.contains_key("spent"));
    }

    #[test]
    fn test_build_provider_json_includes_derived_reset_fields() {
        let data = UsageData {
            provider: "claude".into(),
            entries: vec![UsageEntry {
                label: "session".into(),
                percent_used: 42,
                percent_kind: PercentKind::Used,
                reset_info: "Resets 2pm".into(),
                percent_remaining: 58,
                reset_minutes: Some(90),
                spent: None,
                requests: None,
            }],
        };

        let json = build_provider_json(&data);
        let obj = json.as_object().unwrap();
        let entry = obj["session"].as_object().unwrap();
        assert_eq!(entry["reset_minutes"], 90);
        assert_eq!(entry["reset_hours"], serde_json::json!(1.5));
        assert_eq!(entry["reset_days"], serde_json::json!(0.06));
    }
}
