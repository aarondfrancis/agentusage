#![deny(warnings)]

use anyhow::Result;
use clap::Parser;
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::Ordering;

use agentusage::{
    run_all, run_claude, run_codex, run_gemini, AllResults, ApprovalPolicy, PercentKind,
    UsageConfig, UsageData,
};

#[derive(Parser)]
#[command(
    name = "agentusage",
    version,
    about = "Check Claude Code, Codex, and Gemini CLI usage limits",
    long_about = "Check Claude Code, Codex, and Gemini CLI usage limits.\n\n\
        Launches each CLI tool in a tmux session, runs its usage/limits command,\n\
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
  agentusage --cleanup        Kill stale tmux sessions and exit

Exit codes:
  0  Success
  1  General error
  2  Required tool not found (tmux or provider CLI)
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

    /// Kill all stale agentusage tmux sessions and exit
    #[arg(long)]
    cleanup: bool,

    /// Check if tmux and provider CLIs are installed
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

    // Check tmux
    match Command::new("tmux").arg("-V").output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            println!("  tmux: {}", version.trim());
        }
        Ok(_) => {
            println!("  tmux: installed (unknown version)");
        }
        Err(_) => {
            println!("  tmux: not found");
            println!("         Install with: brew install tmux (macOS) or apt install tmux (Linux)");
            all_ok = false;
        }
    }

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
        println!("\nAll dependencies found.");
    } else {
        println!("\nSome dependencies are missing.");
        std::process::exit(1);
    }
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
        let (display_pct, kind) = match entry.percent_kind {
            PercentKind::Used => (entry.percent_used, "used"),
            PercentKind::Left => (entry.percent_remaining, "left"),
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
            display_pct,
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

/// Build a JSON object for a single provider: { label: { ...fields }, ... }
fn build_provider_json(data: &UsageData) -> serde_json::Value {
    let mut entries = serde_json::Map::new();
    for entry in &data.entries {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "percent_used".into(),
            serde_json::json!(entry.percent_used),
        );
        obj.insert(
            "percent_remaining".into(),
            serde_json::json!(entry.percent_remaining),
        );
        obj.insert("reset_info".into(), serde_json::json!(entry.reset_info));
        if let Some(mins) = entry.reset_minutes {
            obj.insert("reset_minutes".into(), serde_json::json!(mins));
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentusage::UsageEntry;

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
        // spent is None, should be absent
        assert!(!entry.contains_key("spent"));
    }
}

fn main() {
    let cli = Cli::parse();

    // Handle --cleanup
    if cli.cleanup {
        agentusage::tmux::TmuxSession::kill_all_stale_sessions();
        return;
    }

    // Handle --doctor
    if cli.doctor {
        run_doctor();
        return;
    }

    // Set up Ctrl+C handler
    ctrlc::set_handler(|| {
        agentusage::tmux::SHUTDOWN.store(true, Ordering::SeqCst);
        agentusage::tmux::kill_registered_sessions();
        std::process::exit(130);
    })
    .expect("Failed to set Ctrl+C handler");

    let config = cli.to_config();

    if cli.claude || cli.codex || cli.gemini {
        // Single provider mode
        let result = if cli.claude {
            run_claude(&config)
        } else if cli.codex {
            run_codex(&config)
        } else {
            run_gemini(&config)
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
        // All providers mode
        let all = run_all(&config);

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
