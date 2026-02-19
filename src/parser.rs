use anyhow::Result;
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use regex::Regex;

use crate::types::{PercentKind, UsageData, UsageEntry};

/// Parse Claude Code `/status` Usage tab output.
pub fn parse_claude_output(text: &str) -> Result<UsageData> {
    let pct_re = Regex::new(r"(\d+(?:\.\d+)?)\s*%\s*used")?;
    let money_re = Regex::new(r"(\$[\d.,]+\s*/\s*\$[\d.,]+\s*spent)")?;
    let reset_re = Regex::new(r"((?:Resets?|Reses)\s*.+)")?;

    fn normalize_reset_text(raw: &str) -> String {
        let trimmed = raw.trim();
        let mut chars = trimmed.chars();
        let prefix: String = chars.by_ref().take(5).collect();
        if prefix.eq_ignore_ascii_case("reses") {
            let rest: String = chars.collect();
            format!("Resets{}", rest)
        } else {
            trimmed.to_string()
        }
    }

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
            for line in &lines[(i + 1)..scan_end] {
                let line = line.trim();

                if percent.is_none() {
                    if let Some(caps) = pct_re.captures(line) {
                        match caps[1].parse::<f64>() {
                            Ok(v) => percent = Some(v),
                            Err(e) => {
                                eprintln!(
                                    "Warning: skipping unparseable percentage '{}': {}",
                                    &caps[1], e
                                );
                            }
                        }
                    }
                }

                if reset_info.is_empty() {
                    if let Some(caps) = reset_re.captures(line) {
                        reset_info = normalize_reset_text(&caps[1]);
                    }
                }

                if spent.is_none() {
                    if let Some(caps) = money_re.captures(line) {
                        spent = Some(caps[1].to_string());
                    }
                }
            }

            if let Some(pct) = percent {
                let reset_minutes = parse_reset_minutes(&reset_info, "claude");
                let used = (pct.round() as u32).min(100);
                entries.push(UsageEntry {
                    label,
                    percent_used: used,
                    percent_remaining: 100 - used,
                    percent_kind: PercentKind::Used,
                    reset_info,
                    reset_minutes,
                    spent,
                    requests: None,
                });
            }
        }

        i += 1;
    }

    // Fallback for noisy PTY captures where section labels can be partially overwritten.
    // In that case, recover by ordering percentages as session/week/sonnet/extra.
    if entries.is_empty() {
        let labels = [
            "Current session",
            "Current week (all models)",
            "Current week (Sonnet only)",
            "Extra usage",
        ];
        let percents: Vec<f64> = pct_re
            .captures_iter(text)
            .filter_map(|caps| caps[1].parse::<f64>().ok())
            .collect();
        let resets: Vec<String> = reset_re
            .captures_iter(text)
            .map(|caps| normalize_reset_text(&caps[1]))
            .collect();
        let spent = money_re
            .captures(text)
            .map(|caps| caps[1].trim().to_string());

        for (idx, pct) in percents.into_iter().take(labels.len()).enumerate() {
            let used = (pct.round() as u32).min(100);
            let reset_info = resets.get(idx).cloned().unwrap_or_default();
            entries.push(UsageEntry {
                label: labels[idx].to_string(),
                percent_used: used,
                percent_remaining: 100 - used,
                percent_kind: PercentKind::Used,
                reset_minutes: parse_reset_minutes(&reset_info, "claude"),
                reset_info,
                spent: if idx == 3 { spent.clone() } else { None },
                requests: None,
            });
        }
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
        r"^\s*([\w][\w\s.-]*?)\s*limit:\s+\[.*?\]\s+(\d+(?:\.\d+)?)\s*%\s*(left|used)\s+\(resets?\s+(.+?)\)",
    )?;
    // Section header: "Something limit:" on its own line (no progress bar)
    let section_re = Regex::new(r"^\s*([\w][\w\s.-]+?)\s*limit:\s*$")?;

    let mut entries = Vec::new();
    let mut current_section: Option<String> = None;

    for raw_line in text.lines() {
        // Strip box-drawing characters (│, ╭, ╰, ╮, ╯) from line start/end
        let line = raw_line
            .trim()
            .trim_start_matches('│')
            .trim_end_matches('│')
            .trim();

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
                    eprintln!(
                        "Warning: skipping unparseable Codex percentage '{}': {}",
                        &caps[2], e
                    );
                    continue;
                }
            };
            let percent_kind = if &caps[3] == "left" {
                PercentKind::Left
            } else {
                PercentKind::Used
            };
            let reset_info = format!("resets {}", &caps[4]);

            let clamped = (percent.round() as u32).min(100);
            let (percent_used, percent_remaining) = match percent_kind {
                PercentKind::Used => (clamped, 100 - clamped),
                PercentKind::Left => (100 - clamped, clamped),
            };
            let reset_minutes = parse_reset_minutes(&reset_info, "codex");
            entries.push(UsageEntry {
                label,
                percent_used,
                percent_remaining,
                percent_kind,
                reset_info,
                reset_minutes,
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
            && !line.contains(':')
        // Key-value metadata lines like "Model:", "Account:"
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
        r"^\s*(gemini-[\w.-]+)\s+(\d+|-)\s+(\d+(?:\.\d+)?)\s*%\s*\(Resets?\s+in\s+(.+?)\)",
    )?;

    let mut entries = Vec::new();

    for raw_line in text.lines() {
        // Strip box-drawing characters
        let line = raw_line
            .trim()
            .trim_start_matches('│')
            .trim_end_matches('│')
            .trim();

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
                    eprintln!(
                        "Warning: skipping unparseable Gemini percentage '{}': {}",
                        &caps[3], e
                    );
                    continue;
                }
            };
            let reset_info = format!("Resets in {}", &caps[4]);

            let reset_minutes = parse_reset_minutes(&reset_info, "gemini");
            let clamped = (percent.round() as u32).min(100);
            entries.push(UsageEntry {
                label,
                percent_used: 100 - clamped,
                percent_remaining: clamped,
                percent_kind: PercentKind::Left,
                reset_info,
                reset_minutes,
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

// ── Reset time parsing ──────────────────────────────────────────

fn parse_month(s: &str) -> Option<u32> {
    match s.to_lowercase().as_str() {
        "jan" | "january" => Some(1),
        "feb" | "february" => Some(2),
        "mar" | "march" => Some(3),
        "apr" | "april" => Some(4),
        "may" => Some(5),
        "jun" | "june" => Some(6),
        "jul" | "july" => Some(7),
        "aug" | "august" => Some(8),
        "sep" | "september" => Some(9),
        "oct" | "october" => Some(10),
        "nov" | "november" => Some(11),
        "dec" | "december" => Some(12),
        _ => None,
    }
}

fn parse_12h_time(s: &str) -> Option<(u32, u32)> {
    let re = Regex::new(r"(?i)(\d{1,2})(?::(\d{2}))?\s*(am|pm)").ok()?;
    let caps = re.captures(s)?;
    let mut hour: u32 = caps[1].parse().ok()?;
    let min: u32 = caps
        .get(2)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let ampm = caps[3].to_lowercase();

    if ampm == "pm" && hour != 12 {
        hour += 12;
    } else if ampm == "am" && hour == 12 {
        hour = 0;
    }

    if hour > 23 || min > 59 {
        return None;
    }

    Some((hour, min))
}

fn parse_gemini_reset(reset_info: &str) -> Option<i64> {
    // "Resets in 3h 3m"
    let re_hm = Regex::new(r"(\d+)h\s*(\d+)m").ok()?;
    if let Some(caps) = re_hm.captures(reset_info) {
        let hours: i64 = caps[1].parse().ok()?;
        let minutes: i64 = caps[2].parse().ok()?;
        return Some(hours * 60 + minutes);
    }
    // "Resets in 3h"
    let re_h = Regex::new(r"(\d+)h").ok()?;
    if let Some(caps) = re_h.captures(reset_info) {
        let hours: i64 = caps[1].parse().ok()?;
        return Some(hours * 60);
    }
    // "Resets in 45m"
    let re_m = Regex::new(r"(\d+)m").ok()?;
    if let Some(caps) = re_m.captures(reset_info) {
        let minutes: i64 = caps[1].parse().ok()?;
        return Some(minutes);
    }
    None
}

fn parse_codex_reset(reset_info: &str, now_utc: DateTime<Utc>) -> Option<i64> {
    // "resets 12:07 on 16 Feb"
    let re_with_date =
        Regex::new(r"(?i)resets?\s+(\d{1,2}):(\d{2})\s+on\s+(\d{1,2})\s+(\w+)").ok()?;
    if let Some(caps) = re_with_date.captures(reset_info) {
        let hour: u32 = caps[1].parse().ok()?;
        let min: u32 = caps[2].parse().ok()?;
        let day: u32 = caps[3].parse().ok()?;
        let month = parse_month(&caps[4])?;

        let now_local = now_utc.with_timezone(&Local);
        let year = now_local.date_naive().year();

        let mut reset_date = NaiveDate::from_ymd_opt(year, month, day)?;
        if reset_date < now_local.date_naive() {
            reset_date = NaiveDate::from_ymd_opt(year + 1, month, day)?;
        }
        let reset_time = NaiveTime::from_hms_opt(hour, min, 0)?;
        let reset_naive = reset_date.and_time(reset_time);
        let reset_local = reset_naive.and_local_timezone(Local).single()?;
        let reset_utc = reset_local.with_timezone(&Utc);

        let minutes = reset_utc.signed_duration_since(now_utc).num_minutes();
        if minutes < 0 {
            return None;
        }
        return Some(minutes);
    }

    // "resets 16:25"
    let re_time = Regex::new(r"(?i)resets?\s+(\d{1,2}):(\d{2})").ok()?;
    if let Some(caps) = re_time.captures(reset_info) {
        let hour: u32 = caps[1].parse().ok()?;
        let min: u32 = caps[2].parse().ok()?;

        let now_local = now_utc.with_timezone(&Local);
        let today = now_local.date_naive();
        let reset_time = NaiveTime::from_hms_opt(hour, min, 0)?;

        let reset_naive = today.and_time(reset_time);
        let reset_local = reset_naive.and_local_timezone(Local).single()?;
        let mut reset_utc = reset_local.with_timezone(&Utc);

        if reset_utc <= now_utc {
            let tomorrow = today.succ_opt()?;
            let reset_naive = tomorrow.and_time(reset_time);
            let reset_local = reset_naive.and_local_timezone(Local).single()?;
            reset_utc = reset_local.with_timezone(&Utc);
        }

        return Some(reset_utc.signed_duration_since(now_utc).num_minutes());
    }

    None
}

fn parse_claude_reset(reset_info: &str, now_utc: DateTime<Utc>) -> Option<i64> {
    // Extract timezone from parentheses
    let tz_re = Regex::new(r"\(([^)]+)\)").ok()?;
    let tz_str = tz_re.captures(reset_info)?.get(1)?.as_str();
    let tz: Tz = tz_str.parse().ok()?;

    let now_tz = now_utc.with_timezone(&tz);

    // "Resets Feb 20 at 9am (...)" or compact "ResetsFeb20at9am(...)"
    let date_time_re =
        Regex::new(r"(?i)Resets?\s*([A-Za-z]+)\s*(\d{1,2})\s*at\s*(.+?)\s*\(").ok()?;
    if let Some(caps) = date_time_re.captures(reset_info) {
        let month = parse_month(&caps[1])?;
        let day: u32 = caps[2].parse().ok()?;
        let (hour, min) = parse_12h_time(&caps[3])?;

        let year = now_tz.date_naive().year();
        let mut reset_date = NaiveDate::from_ymd_opt(year, month, day)?;
        if reset_date < now_tz.date_naive() {
            reset_date = NaiveDate::from_ymd_opt(year + 1, month, day)?;
        }
        let reset_time = NaiveTime::from_hms_opt(hour, min, 0)?;
        let reset_naive = reset_date.and_time(reset_time);
        let reset_tz = reset_naive.and_local_timezone(tz).single()?;
        let reset_utc = reset_tz.with_timezone(&Utc);

        let minutes = reset_utc.signed_duration_since(now_utc).num_minutes();
        if minutes < 0 {
            return None;
        }
        return Some(minutes);
    }

    // "Resets 2pm (...)" or compact "Resets10pm(...)".
    // Time only: assume today in provider TZ and wrap to tomorrow if already past.
    let time_re = Regex::new(r"(?i)Resets?\s*(\d{1,2}(?::\d{2})?\s*(?:am|pm))\s*\(").ok()?;
    if let Some(caps) = time_re.captures(reset_info) {
        let (hour, min) = parse_12h_time(&caps[1])?;

        let today = now_tz.date_naive();
        let reset_time = NaiveTime::from_hms_opt(hour, min, 0)?;
        let reset_naive = today.and_time(reset_time);
        let reset_tz_dt = reset_naive.and_local_timezone(tz).single()?;
        let mut reset_utc = reset_tz_dt.with_timezone(&Utc);

        if reset_utc <= now_utc {
            let tomorrow = today.succ_opt()?;
            let reset_naive = tomorrow.and_time(reset_time);
            let reset_tz_dt = reset_naive.and_local_timezone(tz).single()?;
            reset_utc = reset_tz_dt.with_timezone(&Utc);
        }

        return Some(reset_utc.signed_duration_since(now_utc).num_minutes());
    }

    // "Resets Mar 1 (...)" or compact "ResetsMar1(...)" - date only
    let date_re = Regex::new(r"(?i)Resets?\s*([A-Za-z]+)\s*(\d{1,2})\s*\(").ok()?;
    if let Some(caps) = date_re.captures(reset_info) {
        let month = parse_month(&caps[1])?;
        let day: u32 = caps[2].parse().ok()?;

        let year = now_tz.date_naive().year();
        let mut reset_date = NaiveDate::from_ymd_opt(year, month, day)?;
        if reset_date < now_tz.date_naive() {
            reset_date = NaiveDate::from_ymd_opt(year + 1, month, day)?;
        }
        let reset_time = NaiveTime::from_hms_opt(0, 0, 0)?;
        let reset_naive = reset_date.and_time(reset_time);
        let reset_tz_dt = reset_naive.and_local_timezone(tz).single()?;
        let reset_utc = reset_tz_dt.with_timezone(&Utc);

        let minutes = reset_utc.signed_duration_since(now_utc).num_minutes();
        if minutes < 0 {
            return None;
        }
        return Some(minutes);
    }

    None
}

/// Parse reset_info into minutes until reset. Testable variant that accepts a controlled "now".
fn parse_reset_minutes_at(reset_info: &str, provider: &str, now_utc: DateTime<Utc>) -> Option<i64> {
    if reset_info.is_empty() {
        return None;
    }
    match provider {
        "gemini" => parse_gemini_reset(reset_info),
        "codex" => parse_codex_reset(reset_info, now_utc),
        "claude" => parse_claude_reset(reset_info, now_utc),
        _ => None,
    }
}

/// Parse reset_info string into minutes until reset.
pub fn parse_reset_minutes(reset_info: &str, provider: &str) -> Option<i64> {
    parse_reset_minutes_at(reset_info, provider, Utc::now())
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
        assert_eq!(data.entries[0].percent_used, 1);
        assert_eq!(data.entries[0].percent_kind, PercentKind::Used);
        assert!(data.entries[0].reset_info.contains("Resets 2pm"));

        assert_eq!(data.entries[1].label, "Current week (all models)");
        assert_eq!(data.entries[1].percent_used, 0);

        assert_eq!(data.entries[2].label, "Current week (Sonnet only)");

        assert_eq!(data.entries[3].label, "Extra usage");
        assert_eq!(data.entries[3].percent_used, 15);
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
        assert_eq!(data.entries[0].percent_used, 13);
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
        assert!(data.entries[0]
            .spent
            .as_ref()
            .unwrap()
            .contains("$1,234.56"));
    }

    #[test]
    fn test_claude_with_leading_whitespace() {
        let text = "   Current session\n   ██░░  10% used\n   Resets 5pm (US/Eastern)\n";
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent_used, 10);
        assert!(data.entries[0].reset_info.contains("Resets 5pm"));
    }

    #[test]
    fn test_claude_noisy_tui_fallback_ordered_percents() {
        let text = r#"
Settings:StatusConfigUsage
Loadingusagedata…
Curretsession    ██████████████28%usedResets7pm(America/Chicago)
Currentweek(allmodels)████████16%usedResetsFeb20at9am(America/Chicago)
Currentweek(Sonnetonly)0%usedResetsFeb15at11am(America/Chicago)
Extrausage███████▊15%used
$77.33/$500.00spent·ResetsMar1(America/Chicago)
"#;
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 4);
        assert_eq!(data.entries[0].label, "Current session");
        assert_eq!(data.entries[0].percent_used, 28);
        assert_eq!(data.entries[1].label, "Current week (all models)");
        assert_eq!(data.entries[1].percent_used, 16);
        assert_eq!(data.entries[2].label, "Current week (Sonnet only)");
        assert_eq!(data.entries[2].percent_used, 0);
        assert_eq!(data.entries[3].label, "Extra usage");
        assert_eq!(data.entries[3].percent_used, 15);
    }

    #[test]
    fn test_claude_reset_on_same_line_as_spent() {
        let text =
            "Extra usage\n██  15% used\n$77.33 / $500.00 spent · Resets Mar 1 (America/Chicago)\n";
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
                percent_used: 5,
                percent_kind: PercentKind::Used,
                reset_info: "Resets 2pm".to_string(),
                percent_remaining: 95,
                reset_minutes: None,
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
                percent_used: 15,
                percent_kind: PercentKind::Used,
                reset_info: "Resets Mar 1".to_string(),
                percent_remaining: 85,
                reset_minutes: None,
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
        assert_eq!(data.entries[0].percent_remaining, 97);
        assert_eq!(data.entries[0].percent_kind, PercentKind::Left);
        assert_eq!(data.entries[0].reset_info, "resets 11:07");

        assert_eq!(data.entries[1].label, "Weekly limit");
        assert_eq!(data.entries[1].percent_remaining, 71);
        assert_eq!(data.entries[1].reset_info, "resets 12:07 on 16 Feb");

        assert_eq!(data.entries[2].label, "GPT-5.3-Codex-Spark 5h limit");
        assert_eq!(data.entries[2].percent_remaining, 100);

        assert_eq!(data.entries[3].label, "GPT-5.3-Codex-Spark Weekly limit");
        assert_eq!(data.entries[3].percent_remaining, 100);
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
        assert_eq!(data.entries[0].percent_remaining, 50);
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
        assert_eq!(data.entries[0].percent_remaining, 80);
    }

    #[test]
    fn test_codex_decimal_percentage() {
        let text = "Weekly limit:  [██] 33.5% left (resets 12:00 on 20 Feb)\n";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent_remaining, 34);
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
        assert_eq!(data.entries[0].percent_remaining, 80);
    }

    #[test]
    fn test_codex_json_serialization_percent_left() {
        let data = crate::types::UsageData {
            provider: "codex".to_string(),
            entries: vec![crate::types::UsageEntry {
                label: "5h limit".to_string(),
                percent_used: 3,
                percent_kind: PercentKind::Left,
                reset_info: "resets 11:07".to_string(),
                percent_remaining: 97,
                reset_minutes: None,
                spent: None,
                requests: None,
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"codex\""));
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
        assert_eq!(data.entries[0].percent_remaining, 100);
        assert_eq!(data.entries[0].percent_kind, PercentKind::Left);
        assert_eq!(data.entries[0].requests, Some("2".to_string()));
        assert_eq!(data.entries[0].reset_info, "Resets in 23h 58m");

        assert_eq!(data.entries[1].label, "gemini-3-flash-preview");
        assert_eq!(data.entries[1].requests, Some("4".to_string()));

        assert_eq!(data.entries[3].label, "gemini-2.5-pro");
        assert_eq!(data.entries[3].percent_remaining, 98);
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
        assert_eq!(data.entries[0].percent_remaining, 95);
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
        assert_eq!(data.entries[0].percent_remaining, 100);
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
        assert_eq!(
            data1.entries[0].percent_remaining,
            data2.entries[0].percent_remaining
        );
    }

    #[test]
    fn test_gemini_json_serialization() {
        let data = crate::types::UsageData {
            provider: "gemini".to_string(),
            entries: vec![crate::types::UsageEntry {
                label: "gemini-2.5-flash".to_string(),
                percent_used: 1,
                percent_kind: PercentKind::Left,
                reset_info: "Resets in 4h 49m".to_string(),
                percent_remaining: 99,
                reset_minutes: Some(289),
                spent: None,
                requests: Some("6".to_string()),
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"gemini\""));
        assert!(json.contains("\"percent_remaining\":99"));
        assert!(json.contains("\"requests\":\"6\""));
        assert!(!json.contains("spent"));
    }

    #[test]
    fn test_gemini_json_skips_none_requests() {
        let data = crate::types::UsageData {
            provider: "gemini".to_string(),
            entries: vec![crate::types::UsageEntry {
                label: "gemini-2.5-pro".to_string(),
                percent_used: 2,
                percent_kind: PercentKind::Left,
                reset_info: "Resets in 2h 35m".to_string(),
                percent_remaining: 98,
                reset_minutes: Some(155),
                spent: None,
                requests: None,
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(!json.contains("requests"));
        assert!(!json.contains("spent"));
    }

    // ── Percentage clamping tests ─────────────────────────────────

    #[test]
    fn test_claude_percentage_over_100_clamped() {
        let text = "Current session\n██░░  105% used\nResets 2pm (America/Chicago)\n";
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent_used, 100);
        assert_eq!(data.entries[0].percent_remaining, 0);
    }

    #[test]
    fn test_codex_percentage_over_100_used_clamped() {
        let text = "5h limit:  [████] 110% used (resets 14:00)\n";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent_used, 100);
        assert_eq!(data.entries[0].percent_remaining, 0);
    }

    #[test]
    fn test_codex_percentage_over_100_left_clamped() {
        let text = "5h limit:  [████] 105% left (resets 14:00)\n";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent_remaining, 100);
        assert_eq!(data.entries[0].percent_used, 0);
    }

    #[test]
    fn test_gemini_percentage_over_100_clamped() {
        let text = "│  gemini-2.5-flash   3   105.0% (Resets in 1h 30m)\n";
        let data = parse_gemini_output(text).unwrap();
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].percent_remaining, 100);
        assert_eq!(data.entries[0].percent_used, 0);
    }

    // ── Year rollover tests ─────────────────────────────────────────

    #[test]
    fn test_codex_reset_year_rollover() {
        use chrono::TimeZone;
        // Dec 31, 2026 at 23:00 UTC. "resets 10:00 on 2 Jan" should be Jan 2, 2027
        let now = Utc.with_ymd_and_hms(2026, 12, 31, 23, 0, 0).unwrap();
        let result = parse_reset_minutes_at("resets 10:00 on 2 Jan", "codex", now);
        assert!(result.is_some());
        assert!(result.unwrap() > 0);
    }

    #[test]
    fn test_claude_reset_date_time_year_rollover() {
        use chrono::TimeZone;
        // Dec 31, 2026 at 23:00 UTC. "Resets Jan 5 at 9am (UTC)" should be Jan 5, 2027
        let now = Utc.with_ymd_and_hms(2026, 12, 31, 23, 0, 0).unwrap();
        let result = parse_reset_minutes_at("Resets Jan 5 at 9am (UTC)", "claude", now);
        assert!(result.is_some());
        assert!(result.unwrap() > 0);
    }

    #[test]
    fn test_claude_reset_date_only_year_rollover() {
        use chrono::TimeZone;
        // Dec 31, 2026 at 23:00 UTC. "Resets Jan 10 (UTC)" should be Jan 10, 2027
        let now = Utc.with_ymd_and_hms(2026, 12, 31, 23, 0, 0).unwrap();
        let result = parse_reset_minutes_at("Resets Jan 10 (UTC)", "claude", now);
        assert!(result.is_some());
        assert!(result.unwrap() > 0);
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

    // ── Normalized / reset_minutes tests ────────────────────────────

    #[test]
    fn test_gemini_reset_minutes_hours_and_minutes() {
        assert_eq!(parse_reset_minutes("Resets in 3h 3m", "gemini"), Some(183));
    }

    #[test]
    fn test_gemini_reset_minutes_hours_only() {
        assert_eq!(parse_reset_minutes("Resets in 5h", "gemini"), Some(300));
    }

    #[test]
    fn test_gemini_reset_minutes_minutes_only() {
        assert_eq!(parse_reset_minutes("Resets in 45m", "gemini"), Some(45));
    }

    #[test]
    fn test_gemini_reset_minutes_large() {
        assert_eq!(
            parse_reset_minutes("Resets in 23h 58m", "gemini"),
            Some(1438)
        );
    }

    #[test]
    fn test_reset_minutes_empty_string() {
        assert_eq!(parse_reset_minutes("", "gemini"), None);
        assert_eq!(parse_reset_minutes("", "codex"), None);
        assert_eq!(parse_reset_minutes("", "claude"), None);
    }

    #[test]
    fn test_reset_minutes_unparseable() {
        assert_eq!(parse_reset_minutes("some garbage", "claude"), None);
        assert_eq!(parse_reset_minutes("no time here", "codex"), None);
        assert_eq!(parse_reset_minutes("nothing useful", "gemini"), None);
    }

    #[test]
    fn test_reset_minutes_unknown_provider() {
        assert_eq!(parse_reset_minutes("Resets in 3h 3m", "unknown"), None);
    }

    #[test]
    fn test_claude_reset_minutes_time_with_tz() {
        use chrono::TimeZone;
        // 12:00 UTC on Feb 13, 2026. America/Chicago is CST (UTC-6) in February.
        // "Resets 2pm (America/Chicago)" = 14:00 CST = 20:00 UTC
        // Delta = 8 hours = 480 minutes
        let now = Utc.with_ymd_and_hms(2026, 2, 13, 12, 0, 0).unwrap();
        let result = parse_reset_minutes_at("Resets 2pm (America/Chicago)", "claude", now);
        assert_eq!(result, Some(480));
    }

    #[test]
    fn test_claude_reset_minutes_date_time_with_tz() {
        use chrono::TimeZone;
        // 12:00 UTC on Feb 13, 2026
        // "Resets Feb 20 at 9am (America/Chicago)" = 9:00 CST = 15:00 UTC on Feb 20
        // Delta = 7 days + 3 hours = 10260 minutes
        let now = Utc.with_ymd_and_hms(2026, 2, 13, 12, 0, 0).unwrap();
        let result =
            parse_reset_minutes_at("Resets Feb 20 at 9am (America/Chicago)", "claude", now);
        assert_eq!(result, Some(7 * 24 * 60 + 3 * 60));
    }

    #[test]
    fn test_claude_reset_minutes_date_only_with_tz() {
        use chrono::TimeZone;
        // 12:00 UTC on Feb 13, 2026
        // "Resets Mar 1 (America/Chicago)" = 00:00 CST Mar 1 = 06:00 UTC Mar 1
        // Delta from Feb 13 12:00 UTC to Mar 1 06:00 UTC = 15 days 18 hours = 22680 min
        let now = Utc.with_ymd_and_hms(2026, 2, 13, 12, 0, 0).unwrap();
        let result = parse_reset_minutes_at("Resets Mar 1 (America/Chicago)", "claude", now);
        assert_eq!(result, Some(15 * 24 * 60 + 18 * 60));
    }

    #[test]
    fn test_claude_reset_minutes_compact_time_with_tz() {
        use chrono::TimeZone;
        // 12:00 UTC on Feb 13, 2026. 10pm CST is 04:00 UTC next day.
        // Delta = 16 hours = 960 minutes
        let now = Utc.with_ymd_and_hms(2026, 2, 13, 12, 0, 0).unwrap();
        let result = parse_reset_minutes_at("Resets10pm(America/Chicago)", "claude", now);
        assert_eq!(result, Some(16 * 60));
    }

    #[test]
    fn test_claude_reset_minutes_compact_date_time_with_tz() {
        use chrono::TimeZone;
        // 12:00 UTC on Feb 13, 2026
        // "ResetsFeb20at9am(America/Chicago)" = 9:00 CST = 15:00 UTC on Feb 20
        // Delta = 7 days + 3 hours = 10260 minutes
        let now = Utc.with_ymd_and_hms(2026, 2, 13, 12, 0, 0).unwrap();
        let result = parse_reset_minutes_at("ResetsFeb20at9am(America/Chicago)", "claude", now);
        assert_eq!(result, Some(7 * 24 * 60 + 3 * 60));
    }

    #[test]
    fn test_claude_reset_minutes_compact_date_only_with_tz() {
        use chrono::TimeZone;
        // 12:00 UTC on Feb 13, 2026
        // "ResetsMar1(America/Chicago)" = 00:00 CST Mar 1 = 06:00 UTC Mar 1
        // Delta from Feb 13 12:00 UTC to Mar 1 06:00 UTC = 15 days 18 hours = 22680 min
        let now = Utc.with_ymd_and_hms(2026, 2, 13, 12, 0, 0).unwrap();
        let result = parse_reset_minutes_at("ResetsMar1(America/Chicago)", "claude", now);
        assert_eq!(result, Some(15 * 24 * 60 + 18 * 60));
    }

    #[test]
    fn test_claude_reset_minutes_no_tz_returns_none() {
        // No timezone in parentheses → cannot compute
        assert_eq!(parse_reset_minutes("Resets 2pm", "claude"), None);
    }

    #[test]
    fn test_claude_reset_minutes_past_time_wraps_to_tomorrow() {
        use chrono::TimeZone;
        // 22:00 UTC on Feb 13, 2026. America/Chicago is CST (UTC-6).
        // Local time = 16:00 CST. "Resets 2pm" = 14:00 CST = already past today.
        // Should wrap to tomorrow: 14:00 CST Feb 14 = 20:00 UTC Feb 14
        // Delta from 22:00 UTC Feb 13 to 20:00 UTC Feb 14 = 22 hours = 1320 minutes
        let now = Utc.with_ymd_and_hms(2026, 2, 13, 22, 0, 0).unwrap();
        let result = parse_reset_minutes_at("Resets 2pm (America/Chicago)", "claude", now);
        assert_eq!(result, Some(22 * 60));
    }

    #[test]
    fn test_codex_reset_minutes_time_only() {
        use chrono::Timelike;
        // Construct a reset time ~2 hours from now in local time
        let now = Utc::now();
        let local_now = now.with_timezone(&Local);
        let future_hour = (local_now.hour() + 2) % 24;
        let future_min = local_now.minute();
        let reset_str = format!("resets {:02}:{:02}", future_hour, future_min);

        let result = parse_reset_minutes_at(&reset_str, "codex", now);
        assert!(result.is_some());
        let mins = result.unwrap();
        assert!((118..=122).contains(&mins), "Expected ~120, got {}", mins);
    }

    #[test]
    fn test_codex_reset_minutes_with_date() {
        use chrono::TimeZone;
        // This test is timezone-dependent but should return Some with positive minutes
        let now = Utc.with_ymd_and_hms(2026, 2, 13, 10, 0, 0).unwrap();
        let result = parse_reset_minutes_at("resets 12:07 on 16 Feb", "codex", now);
        // Result depends on local tz, but Feb 16 is after Feb 13 so should be positive
        assert!(result.is_some());
        assert!(result.unwrap() > 0);
    }

    #[test]
    fn test_normalized_percent_remaining_used() {
        let text = "Current session\n██░░  25% used\nResets 3pm (America/Chicago)\n";
        let data = parse_claude_output(text).unwrap();
        assert_eq!(data.entries[0].percent_remaining, 75);
    }

    #[test]
    fn test_normalized_percent_remaining_left() {
        let text = "5h limit:  [████] 80% left (resets 09:30)\n";
        let data = parse_codex_output(text).unwrap();
        assert_eq!(data.entries[0].percent_remaining, 80);
    }

    #[test]
    fn test_normalized_in_json_output() {
        let data = crate::types::UsageData {
            provider: "gemini".to_string(),
            entries: vec![crate::types::UsageEntry {
                label: "gemini-2.5-flash".to_string(),
                percent_used: 1,
                percent_kind: PercentKind::Left,
                reset_info: "Resets in 4h 49m".to_string(),
                percent_remaining: 99,
                reset_minutes: Some(289),
                spent: None,
                requests: Some("6".to_string()),
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"percent_remaining\":99"));
        assert!(json.contains("\"reset_minutes\":289"));
    }

    #[test]
    fn test_normalized_reset_minutes_null_in_json() {
        let data = crate::types::UsageData {
            provider: "claude".to_string(),
            entries: vec![crate::types::UsageEntry {
                label: "session".to_string(),
                percent_used: 5,
                percent_kind: PercentKind::Used,
                reset_info: "Resets 2pm".to_string(),
                percent_remaining: 95,
                reset_minutes: None,
                spent: None,
                requests: None,
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"percent_remaining\":95"));
        // reset_minutes is None and should be skipped
        assert!(!json.contains("reset_minutes"));
    }

    #[test]
    fn test_gemini_parser_populates_normalized() {
        let text = "│  gemini-2.5-flash   6   99.3% (Resets in 4h 49m)\n";
        let data = parse_gemini_output(text).unwrap();
        assert_eq!(data.entries[0].percent_remaining, 99);
        assert_eq!(data.entries[0].reset_minutes, Some(289));
    }
}
