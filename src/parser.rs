use anyhow::Result;
use regex::Regex;

use crate::types::{PercentKind, UsageData, UsageEntry};

/// Parse Claude Code `/status` Usage tab output.
pub fn parse_claude_output(text: &str) -> Result<UsageData> {
    let pct_re = Regex::new(r"(\d+(?:\.\d+)?)\s*%\s*used")?;
    let money_re = Regex::new(r"(\$[\d.,]+\s*/\s*\$[\d.,]+\s*spent)")?;
    let reset_re = Regex::new(r"(Resets?\s+.+)")?;

    let known_headers = [
        "Current session",
        "Current week (all models)",
        "Current week (Sonnet only)",
        "Extra usage",
    ];

    let lines: Vec<&str> = text.lines().collect();
    let mut entries = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();

        let matched_header = known_headers
            .iter()
            .find(|h| trimmed.starts_with(*h))
            .map(|h| h.to_string());

        let header = matched_header.or_else(|| {
            if trimmed.starts_with("Current week") || trimmed.starts_with("Current session") {
                Some(trimmed.to_string())
            } else {
                None
            }
        });

        if let Some(label) = header {
            let mut percent = None;
            let mut reset_info = String::new();
            let mut spent = None;

            let scan_end = (i + 5).min(lines.len());
            for j in (i + 1)..scan_end {
                let line = lines[j].trim();

                if percent.is_none() {
                    if let Some(caps) = pct_re.captures(line) {
                        match caps[1].parse::<f64>() {
                            Ok(v) => percent = Some(v),
                            Err(e) => {
                                eprintln!("Warning: skipping unparseable percentage '{}': {}", &caps[1], e);
                            }
                        }
                    }
                }

                if reset_info.is_empty() {
                    if let Some(caps) = reset_re.captures(line) {
                        reset_info = caps[1].to_string();
                    }
                }

                if spent.is_none() {
                    if let Some(caps) = money_re.captures(line) {
                        spent = Some(caps[1].to_string());
                    }
                }
            }

            if let Some(pct) = percent {
                entries.push(UsageEntry {
                    label,
                    percent: pct,
                    percent_kind: PercentKind::Used,
                    reset_info,
                    spent,
                    requests: None,
                });
            }
        }

        i += 1;
    }

    Ok(UsageData {
        provider: "claude".to_string(),
        entries,
    })
}

/// Parse Codex `/status` inline output.
///
/// Handles both top-level limits and grouped limits:
/// ```text
/// 5h limit:           [████████        ] 97% left (resets 11:07)
/// Weekly limit:       [████████        ] 71% left (resets 12:07 on 16 Feb)
/// GPT-5.3-Codex-Spark limit:
/// 5h limit:           [████████████████] 100% left (resets 15:16)
/// Weekly limit:       [████████████████] 100% left (resets 10:16 on 20 Feb)
/// ```
pub fn parse_codex_output(text: &str) -> Result<UsageData> {
    let limit_re = Regex::new(
        r"^\s*([\w][\w\s.-]*?)\s*limit:\s+\[.*?\]\s+(\d+(?:\.\d+)?)\s*%\s*(left|used)\s+\(resets?\s+(.+?)\)"
    )?;
    // Section header: "Something limit:" on its own line (no progress bar)
    let section_re = Regex::new(
        r"^\s*([\w][\w\s.-]+?)\s*limit:\s*$"
    )?;

    let mut entries = Vec::new();
    let mut current_section: Option<String> = None;

    for raw_line in text.lines() {
        // Strip box-drawing characters (│, ╭, ╰, ╮, ╯) from line start/end
        let line = raw_line.trim().trim_start_matches('│').trim_end_matches('│').trim();

        if line.is_empty() {
            continue;
        }

        // Check for section header first (e.g. "GPT-5.3-Codex-Spark limit:")
        if let Some(caps) = section_re.captures(line) {
            current_section = Some(caps[1].trim().to_string());
            continue;
        }

        // Check for limit line with progress bar
        if let Some(caps) = limit_re.captures(line) {
            let raw_label = caps[1].trim();
            let label = match &current_section {
                Some(section) => format!("{} {} limit", section, raw_label),
                None => format!("{} limit", raw_label),
            };
            let percent = match caps[2].parse::<f64>() {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Warning: skipping unparseable Codex percentage '{}': {}", &caps[2], e);
                    continue;
                }
            };
            let percent_kind = if &caps[3] == "left" {
                PercentKind::Left
            } else {
                PercentKind::Used
            };
            let reset_info = format!("resets {}", &caps[4]);

            entries.push(UsageEntry {
                label,
                percent,
                percent_kind,
                reset_info,
                spent: None,
                requests: None,
            });
            continue;
        }

        // Non-limit, non-section, non-decoration lines reset section context
        if !line.starts_with('[')
            && !line.starts_with('╭')
            && !line.starts_with('╰')
            && !line.starts_with('>') // Codex header ">_ OpenAI Codex"
            && !line.contains(':')    // Key-value metadata lines like "Model:", "Account:"
        {
            current_section = None;
        }
    }

    Ok(UsageData {
        provider: "codex".to_string(),
        entries,
    })
}

/// Parse Gemini CLI `/stats session` output.
///
/// Handles per-model rows like:
/// ```text
/// │  gemini-2.5-flash-lite          2   99.9% (Resets in 23h 58m)
/// │  gemini-2.5-pro                 -    98.1% (Resets in 2h 35m)
/// ```
pub fn parse_gemini_output(text: &str) -> Result<UsageData> {
    let model_re = Regex::new(
        r"^\s*(gemini-[\w.-]+)\s+(\d+|-)\s+(\d+(?:\.\d+)?)\s*%\s*\(Resets?\s+in\s+(.+?)\)"
    )?;

    let mut entries = Vec::new();

    for raw_line in text.lines() {
        // Strip box-drawing characters
        let line = raw_line.trim().trim_start_matches('│').trim_end_matches('│').trim();

        if line.is_empty() {
            continue;
        }

        if let Some(caps) = model_re.captures(line) {
            let label = caps[1].to_string();
            let requests_raw = caps[2].to_string();
            let requests = if requests_raw == "-" {
                None
            } else {
                Some(requests_raw)
            };
            let percent = match caps[3].parse::<f64>() {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Warning: skipping unparseable Gemini percentage '{}': {}", &caps[3], e);
                    continue;
                }
            };
            let reset_info = format!("Resets in {}", &caps[4]);

            entries.push(UsageEntry {
                label,
                percent,
                percent_kind: PercentKind::Left,
                reset_info,
                spent: None,
                requests,
            });
        }
    }

    Ok(UsageData {
        provider: "gemini".to_string(),
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Claude parser tests ─────────────────────────────────────────

    #[test]
    fn test_claude_typical_output() {
        let text = r#"
Settings:   Status    Config   [Usage]

Current session
████████░░░░░░░░  1% used
Resets 2pm (America/Chicago)

Current week (all models)
░░░░░░░░░░░░░░░░  0% used
Resets Feb 20 at 9am (America/Chicago)

Current week (Sonnet only)
░░░░░░░░░░░░░░░░  0% used
Resets Feb 15 at 11am (America/Chicago)

Extra usage
██░░░░░░░░░░░░░░  15% used
$77.33 / $500.00 spent · Resets Mar 1 (America/Chicago)
"#;

        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.provider, "claude");
        assert_eq!(data.entries.len(), 4);

        assert_eq!(data.entries[0].label, "Current session");
        assert_eq!(data.entries[0].percent, 1.0);
        assert_eq!(data.entries[0].percent_kind, PercentKind::Used);
        assert!(data.entries[0].reset_info.contains("Resets 2pm"));

        assert_eq!(data.entries[1].label, "Current week (all models)");
        assert_eq!(data.entries[1].percent, 0.0);

        assert_eq!(data.entries[2].label, "Current week (Sonnet only)");

        assert_eq!(data.entries[3].label, "Extra usage");
        assert_eq!(data.entries[3].percent, 15.0);
        assert!(data.entries[3].spent.is_some());
        assert!(data.entries[3].spent.as_ref().unwrap().contains("$77.33"));
    }

    #[test]
    fn test_claude_empty_output() {
        let data = parse_claude_output("").unwrap();
        assert!(data.entries.is_empty());
    }

    #[test]
    fn test_claude_decimal_percentage() {
        let text = "Current session\n██░░░░  12.5% used\nResets 3pm (America/Chicago)\n";
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent, 12.5);
    }

    #[test]
    fn test_claude_partial_output_no_extra_usage() {
        let text = r#"
Current session
██░░░░  5% used
Resets 2pm (America/Chicago)

Current week (all models)
░░░░░░  0% used
Resets Feb 20 at 9am (America/Chicago)
"#;
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 2);
        assert!(data.entries.iter().all(|e| e.spent.is_none()));
    }

    #[test]
    fn test_claude_unknown_current_week_variant() {
        let text = "Current week (Opus only)\n░░░░  3% used\nResets Feb 20\n";
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].label, "Current week (Opus only)");
    }

    #[test]
    fn test_claude_header_without_percentage_is_skipped() {
        let text = "Current session\nsome random text\nmore random text\n";
        let data = parse_claude_output(text).unwrap();
        assert!(data.entries.is_empty());
    }

    #[test]
    fn test_claude_money_with_commas() {
        let text = "Extra usage\n██░░  50% used\n$1,234.56 / $5,000.00 spent · Resets Mar 1\n";
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert!(data.entries[0].spent.as_ref().unwrap().contains("$1,234.56"));
    }

    #[test]
    fn test_claude_with_leading_whitespace() {
        let text = "   Current session\n   ██░░  10% used\n   Resets 5pm (US/Eastern)\n";
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent, 10.0);
        assert!(data.entries[0].reset_info.contains("Resets 5pm"));
    }

    #[test]
    fn test_claude_reset_on_same_line_as_spent() {
        let text = "Extra usage\n██  15% used\n$77.33 / $500.00 spent · Resets Mar 1 (America/Chicago)\n";
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert!(data.entries[0].spent.is_some());
        assert!(data.entries[0].reset_info.contains("Resets Mar 1"));
    }

    #[test]
    fn test_claude_no_reset_info() {
        let text = "Current session\n██░░  25% used\n";
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].reset_info, "");
    }

    #[test]
    fn test_claude_garbage_between_sections() {
        let text = r#"
Some random TUI chrome
═══════════════════
Current session
██░░  5% used
Resets 2pm

───────────────
Current week (all models)
░░░░  0% used
Resets Feb 20
"#;
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 2);
    }

    #[test]
    fn test_claude_json_serialization_skips_none_spent() {
        let data = crate::types::UsageData {
            provider: "claude".to_string(),
            entries: vec![crate::types::UsageEntry {
                label: "Current session".to_string(),
                percent: 5.0,
                percent_kind: PercentKind::Used,
                reset_info: "Resets 2pm".to_string(),
                spent: None,
                requests: None,
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(!json.contains("spent"));
    }

    #[test]
    fn test_claude_json_serialization_includes_spent() {
        let data = crate::types::UsageData {
            provider: "claude".to_string(),
            entries: vec![crate::types::UsageEntry {
                label: "Extra usage".to_string(),
                percent: 15.0,
                percent_kind: PercentKind::Used,
                reset_info: "Resets Mar 1".to_string(),
                spent: Some("$77.33 / $500.00 spent".to_string()),
                requests: None,
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("$77.33"));
    }

    // ── Codex parser tests ──────────────────────────────────────────

    #[test]
    fn test_codex_typical_output() {
        let text = r#"
│  >_ OpenAI Codex (v0.101.0)                                                             │
│                                                                                         │
│  Model:                       gpt-5.3-codex (reasoning xhigh, summaries auto)           │
│  Directory:                   ~/Code/ccusage                                            │
│  Account:                     user@example.com (Pro)                                    │
│                                                                                         │
│  5h limit:                    [███████████████████░] 97% left (resets 11:07)            │
│  Weekly limit:                [██████████████░░░░░░] 71% left (resets 12:07 on 16 Feb)  │
│  GPT-5.3-Codex-Spark limit:                                                             │
│  5h limit:                    [████████████████████] 100% left (resets 15:16)           │
│  Weekly limit:                [████████████████████] 100% left (resets 10:16 on 20 Feb) │
"#;

        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.provider, "codex");
        assert_eq!(data.entries.len(), 4);

        assert_eq!(data.entries[0].label, "5h limit");
        assert_eq!(data.entries[0].percent, 97.0);
        assert_eq!(data.entries[0].percent_kind, PercentKind::Left);
        assert_eq!(data.entries[0].reset_info, "resets 11:07");

        assert_eq!(data.entries[1].label, "Weekly limit");
        assert_eq!(data.entries[1].percent, 71.0);
        assert_eq!(data.entries[1].reset_info, "resets 12:07 on 16 Feb");

        assert_eq!(data.entries[2].label, "GPT-5.3-Codex-Spark 5h limit");
        assert_eq!(data.entries[2].percent, 100.0);

        assert_eq!(data.entries[3].label, "GPT-5.3-Codex-Spark Weekly limit");
        assert_eq!(data.entries[3].percent, 100.0);
    }

    #[test]
    fn test_codex_empty_output() {
        let data = parse_codex_output("").unwrap();
        assert!(data.entries.is_empty());
    }

    #[test]
    fn test_codex_single_limit() {
        let text = "5h limit:  [██████] 50% left (resets 14:00)\n";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent, 50.0);
    }

    #[test]
    fn test_codex_no_limit_lines() {
        let text = "Model: gpt-5.3\nDirectory: ~/foo\nAccount: test@test.com\n";
        let data = parse_codex_output(text).unwrap();
        assert!(data.entries.is_empty());
    }

    #[test]
    fn test_codex_with_leading_whitespace() {
        let text = "  5h limit:    [████] 80% left (resets 09:30)\n";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent, 80.0);
    }

    #[test]
    fn test_codex_decimal_percentage() {
        let text = "Weekly limit:  [██] 33.5% left (resets 12:00 on 20 Feb)\n";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent, 33.5);
    }

    #[test]
    fn test_codex_section_header_prefixes_nested_limits() {
        let text = "\
Spark limit:
5h limit:  [████] 100% left (resets 15:00)
Weekly limit:  [████] 90% left (resets 12:00 on 20 Feb)
";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 2);
        assert_eq!(data.entries[0].label, "Spark 5h limit");
        assert_eq!(data.entries[1].label, "Spark Weekly limit");
    }

    #[test]
    fn test_codex_section_context_resets_between_groups() {
        // Top-level limits, then a section, then the section's limits
        let text = "\
5h limit:  [████] 97% left (resets 11:07)
Weekly limit:  [████] 71% left (resets 12:07 on 16 Feb)
GPT-Spark limit:
5h limit:  [████] 100% left (resets 15:16)
";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 3);
        assert_eq!(data.entries[0].label, "5h limit");
        assert_eq!(data.entries[1].label, "Weekly limit");
        assert_eq!(data.entries[2].label, "GPT-Spark 5h limit");
    }

    #[test]
    fn test_codex_section_header_with_no_limits_after() {
        let text = "\
5h limit:  [████] 50% left (resets 11:00)
Some-Model limit:
";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].label, "5h limit");
    }

    #[test]
    fn test_codex_box_drawing_stripped_from_all_positions() {
        // Box chars on both sides, like real codex output
        let text = "│  5h limit:  [████] 80% left (resets 09:30)  │\n";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].label, "5h limit");
        assert_eq!(data.entries[0].percent, 80.0);
    }

    #[test]
    fn test_codex_json_serialization_percent_left() {
        let data = crate::types::UsageData {
            provider: "codex".to_string(),
            entries: vec![crate::types::UsageEntry {
                label: "5h limit".to_string(),
                percent: 97.0,
                percent_kind: PercentKind::Left,
                reset_info: "resets 11:07".to_string(),
                spent: None,
                requests: None,
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"codex\""));
        assert!(json.contains("\"left\""));
        assert!(json.contains("97"));
        assert!(!json.contains("spent"));
    }

    #[test]
    fn test_codex_multiple_sections() {
        let text = "\
Model-A limit:
5h limit:  [████] 100% left (resets 10:00)
Model-B limit:
5h limit:  [████] 50% left (resets 12:00)
";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 2);
        assert_eq!(data.entries[0].label, "Model-A 5h limit");
        assert_eq!(data.entries[1].label, "Model-B 5h limit");
    }

    // ── Gemini parser tests ─────────────────────────────────────────

    #[test]
    fn test_gemini_typical_output() {
        let text = r#"
│  Model Usage                 Reqs                  Usage left
│  ────────────────────────────────────────────────────────────
│  gemini-2.5-flash-lite          2   99.9% (Resets in 23h 58m)
│  gemini-3-flash-preview         4    99.3% (Resets in 4h 49m)
│  gemini-2.5-flash               6    99.3% (Resets in 4h 49m)
│  gemini-2.5-pro                 -    98.1% (Resets in 2h 35m)
│  gemini-3-pro-preview           -    98.1% (Resets in 2h 35m)
"#;

        let data = parse_gemini_output(text).unwrap();
        assert_eq!(data.provider, "gemini");
        assert_eq!(data.entries.len(), 5);

        assert_eq!(data.entries[0].label, "gemini-2.5-flash-lite");
        assert_eq!(data.entries[0].percent, 99.9);
        assert_eq!(data.entries[0].percent_kind, PercentKind::Left);
        assert_eq!(data.entries[0].requests, Some("2".to_string()));
        assert_eq!(data.entries[0].reset_info, "Resets in 23h 58m");

        assert_eq!(data.entries[1].label, "gemini-3-flash-preview");
        assert_eq!(data.entries[1].requests, Some("4".to_string()));

        assert_eq!(data.entries[3].label, "gemini-2.5-pro");
        assert_eq!(data.entries[3].percent, 98.1);
        assert_eq!(data.entries[3].requests, None);

        assert_eq!(data.entries[4].label, "gemini-3-pro-preview");
        assert_eq!(data.entries[4].requests, None);
    }

    #[test]
    fn test_gemini_empty_output() {
        let data = parse_gemini_output("").unwrap();
        assert!(data.entries.is_empty());
    }

    #[test]
    fn test_gemini_single_model() {
        let text = "│  gemini-2.5-flash   3   95.0% (Resets in 1h 30m)\n";
        let data = parse_gemini_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].label, "gemini-2.5-flash");
        assert_eq!(data.entries[0].percent, 95.0);
        assert_eq!(data.entries[0].requests, Some("3".to_string()));
    }

    #[test]
    fn test_gemini_dash_requests() {
        let text = "│  gemini-2.5-pro   -   98.1% (Resets in 2h 35m)\n";
        let data = parse_gemini_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].requests, None);
    }

    #[test]
    fn test_gemini_decimal_percentage() {
        let text = "│  gemini-2.5-flash-lite   2   99.9% (Resets in 23h 58m)\n";
        let data = parse_gemini_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent, 99.9);
    }

    #[test]
    fn test_gemini_box_drawing_stripped() {
        // With and without box-drawing chars
        let text1 = "│  gemini-2.5-flash   6   99.3% (Resets in 4h 49m)  │\n";
        let text2 = "  gemini-2.5-flash   6   99.3% (Resets in 4h 49m)\n";

        let data1 = parse_gemini_output(text1).unwrap();
        let data2 = parse_gemini_output(text2).unwrap();

        assert_eq!(data1.entries.len(), 1);
        assert_eq!(data2.entries.len(), 1);
        assert_eq!(data1.entries[0].label, data2.entries[0].label);
        assert_eq!(data1.entries[0].percent, data2.entries[0].percent);
    }

    #[test]
    fn test_gemini_json_serialization() {
        let data = crate::types::UsageData {
            provider: "gemini".to_string(),
            entries: vec![crate::types::UsageEntry {
                label: "gemini-2.5-flash".to_string(),
                percent: 99.3,
                percent_kind: PercentKind::Left,
                reset_info: "Resets in 4h 49m".to_string(),
                spent: None,
                requests: Some("6".to_string()),
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"gemini\""));
        assert!(json.contains("\"left\""));
        assert!(json.contains("99.3"));
        assert!(json.contains("\"requests\":\"6\""));
        assert!(!json.contains("spent"));
    }

    #[test]
    fn test_gemini_json_skips_none_requests() {
        let data = crate::types::UsageData {
            provider: "gemini".to_string(),
            entries: vec![crate::types::UsageEntry {
                label: "gemini-2.5-pro".to_string(),
                percent: 98.1,
                percent_kind: PercentKind::Left,
                reset_info: "Resets in 2h 35m".to_string(),
                spent: None,
                requests: None,
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(!json.contains("requests"));
        assert!(!json.contains("spent"));
    }

    // ── Parse-error-skip tests ──────────────────────────────────────

    #[test]
    fn test_claude_skips_unparseable_percentage() {
        // Verify that a header without a valid percentage in its scan window is skipped,
        // while a subsequent valid entry is still parsed.
        // The parser scans 5 lines ahead, so we put enough padding between sections.
        let text = "\
Current session
no percentage here
line 2
line 3
line 4
line 5
line 6

Current week (all models)
░░░░  5% used
Resets Feb 20
";
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].label, "Current week (all models)");
    }

    #[test]
    fn test_codex_skips_entry_on_bad_data() {
        // A valid line followed by another valid line — both should parse
        let text = "\
5h limit:  [████] 50% left (resets 11:00)
Weekly limit:  [████] 80% left (resets 12:00 on 20 Feb)
";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 2);
    }

    #[test]
    fn test_gemini_skips_entry_on_bad_data() {
        // Verify valid entries still parse when mixed with non-matching lines
        let text = "\
│  Not a model line at all
│  gemini-2.5-flash   6   99.3% (Resets in 4h 49m)
│  random garbage line
│  gemini-2.5-pro     -   98.1% (Resets in 2h 35m)
";
        let data = parse_gemini_output(text).unwrap();
        assert_eq!(data.entries.len(), 2);
    }
}
