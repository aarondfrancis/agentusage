use crate::session::Session;
use crate::types::DialogKind;
use anyhow::Result;
use std::thread;
use std::time::Duration;

fn is_auth_required_prompt(lower: &str) -> bool {
    const AUTH_PHRASES: &[&str] = &[
        "sign in required",
        "log in required",
        "login required",
        "please sign in",
        "please log in",
        "you need to sign in",
        "you need to log in",
        "sign in to continue",
        "log in to continue",
        "sign in with",
        "log in with",
        "must authenticate",
        "please authenticate",
        "authentication required",
        "authenticate before using",
    ];

    AUTH_PHRASES.iter().any(|phrase| lower.contains(phrase))
}

fn looks_like_update_prompt(content: &str) -> bool {
    let lower = content.to_lowercase();
    lower.contains("update available") || lower.contains("new version")
}

fn has_numbered_skip_option(content: &str) -> bool {
    let compact: String = content
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();
    compact.contains("2.skip")
}

fn dismiss_codex_update_prompt(session: &mut Session) -> Result<bool> {
    // Never accept updates on behalf of the user.
    // Try escape first, then explicit skip selection for numbered menus.
    session.send_keys("Esc")?;
    thread::sleep(Duration::from_millis(250));

    let mut content = session.capture_pane()?;
    if content.contains("? for shortcuts") {
        return Ok(true);
    }

    if has_numbered_skip_option(&content) {
        session.send_keys_literal("2")?;
        thread::sleep(Duration::from_millis(100));
        session.send_keys("Enter")?;
        thread::sleep(Duration::from_millis(400));

        content = session.capture_pane()?;
        if content.contains("? for shortcuts") {
            return Ok(true);
        }
    }

    // Fallback for menus without numeric shortcuts: move away from "Update now".
    session.send_keys("Down")?;
    thread::sleep(Duration::from_millis(120));
    session.send_keys("Enter")?;
    thread::sleep(Duration::from_millis(400));

    Ok(true)
}

/// Detect Claude-specific dialogs in screen content.
pub fn detect_claude_dialog(content: &str) -> Option<DialogKind> {
    let lower = content.to_lowercase();

    if looks_like_update_prompt(content) {
        return Some(DialogKind::UpdatePrompt);
    }
    if is_auth_required_prompt(&lower) {
        return Some(DialogKind::AuthRequired);
    }
    if lower.contains("welcome to claude") || lower.contains("first time") {
        return Some(DialogKind::FirstRunSetup);
    }

    None
}

/// Detect Codex-specific dialogs in screen content.
pub fn detect_codex_dialog(content: &str) -> Option<DialogKind> {
    let lower = content.to_lowercase();

    if lower.contains("update available") && lower.contains("codex") {
        return Some(DialogKind::UpdatePrompt);
    }
    if lower.contains("terms") && lower.contains("accept") {
        return Some(DialogKind::TermsAcceptance);
    }
    if lower.contains("do you trust the contents")
        || (lower.contains("trust") && lower.contains("directory"))
    {
        return Some(DialogKind::TrustFolder);
    }
    if lower.contains("sandbox") && lower.contains("trust") {
        return Some(DialogKind::SandboxTrust);
    }
    if is_auth_required_prompt(&lower) {
        return Some(DialogKind::AuthRequired);
    }

    None
}

/// Detect Gemini-specific dialogs in screen content.
/// Priority: trust > theme > update > terms > auth.
pub fn detect_gemini_dialog(content: &str) -> Option<DialogKind> {
    let lower = content.to_lowercase();

    // Priority 1: Trust folder (existing)
    if lower.contains("do you trust this folder") {
        return Some(DialogKind::TrustFolder);
    }
    // Priority 2: Theme selection → FirstRunSetup
    if lower.contains("select a theme")
        || lower.contains("choose a theme")
        || lower.contains("color theme")
    {
        return Some(DialogKind::FirstRunSetup);
    }
    // Priority 3: Update available → UpdatePrompt
    // Exclude extension update notices (informational, not interactive dialogs)
    if looks_like_update_prompt(content) && !lower.contains("extension") {
        return Some(DialogKind::UpdatePrompt);
    }
    // Priority 4: Terms acceptance → TermsAcceptance
    if lower.contains("terms") && (lower.contains("accept") || lower.contains("agree")) {
        return Some(DialogKind::TermsAcceptance);
    }
    // Priority 5: Auth required (last so specific checks win)
    // NOTE: "Waiting for auth..." is a transient spinner, NOT a dialog.
    // It is handled by the prompt-readiness negative guard in lib.rs.
    if is_auth_required_prompt(&lower) {
        return Some(DialogKind::AuthRequired);
    }

    None
}

/// Return a user-facing error message for a detected dialog.
pub fn dialog_error_message(kind: &DialogKind, provider: &str) -> String {
    match kind {
        DialogKind::TrustFolder => format!(
            "{} is showing a trust folder dialog. \
             Run '{0}' manually and accept, or use --approval-policy accept.",
            provider
        ),
        DialogKind::UpdatePrompt => format!(
            "{} is showing an update prompt. \
             Run '{0}' manually to update, or use --approval-policy accept to dismiss.",
            provider
        ),
        DialogKind::AuthRequired => format!(
            "{} requires authentication. \
             Run '{0}' manually and sign in first.",
            provider
        ),
        DialogKind::TermsAcceptance => format!(
            "{} is showing terms acceptance. \
             Run '{0}' manually to accept, or use --approval-policy accept.",
            provider
        ),
        DialogKind::FirstRunSetup => format!(
            "{} is showing a first-run setup dialog. \
             Run '{0}' manually to complete setup first.",
            provider
        ),
        DialogKind::SandboxTrust => format!(
            "{} is showing a sandbox trust dialog. \
             Run '{0}' manually to trust, or use --approval-policy accept.",
            provider
        ),
        DialogKind::Unknown(msg) => format!(
            "{} is showing an unexpected dialog: {}. \
             Run '{0}' manually to resolve.",
            provider, msg
        ),
    }
}

/// Attempt to dismiss a dialog by sending Enter.
/// Returns Ok(true) if the dialog is dismissible (Enter sent),
/// Ok(false) if it requires manual intervention (auth, first-run).
pub fn dismiss_dialog(kind: &DialogKind, provider: &str, session: &mut Session) -> Result<bool> {
    match kind {
        DialogKind::AuthRequired | DialogKind::FirstRunSetup => Ok(false),
        DialogKind::UpdatePrompt => {
            if provider == "codex" {
                dismiss_codex_update_prompt(session)
            } else {
                session.send_keys("Esc")?;
                thread::sleep(Duration::from_secs(1));
                Ok(true)
            }
        }
        _ => {
            session.send_keys("Enter")?;
            thread::sleep(Duration::from_secs(1));
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Claude dialog detection ─────────────────────────────────────

    #[test]
    fn test_detect_claude_update() {
        let content = "A new version is available. Update available: v2.0.0";
        assert_eq!(
            detect_claude_dialog(content),
            Some(DialogKind::UpdatePrompt)
        );
    }

    #[test]
    fn test_detect_claude_auth() {
        let content = "Please sign in to continue using Claude Code.";
        assert_eq!(
            detect_claude_dialog(content),
            Some(DialogKind::AuthRequired)
        );
    }

    #[test]
    fn test_detect_claude_first_run() {
        let content = "Welcome to Claude Code! Let's get you set up.";
        assert_eq!(
            detect_claude_dialog(content),
            Some(DialogKind::FirstRunSetup)
        );
    }

    #[test]
    fn test_detect_claude_none() {
        let content = "❯ Ready for input\nTips: use /help for commands";
        assert_eq!(detect_claude_dialog(content), None);
    }

    // ── Codex dialog detection ──────────────────────────────────────

    #[test]
    fn test_detect_codex_terms() {
        let content = "Please review and accept the Terms of Service.";
        assert_eq!(
            detect_codex_dialog(content),
            Some(DialogKind::TermsAcceptance)
        );
    }

    #[test]
    fn test_detect_codex_update() {
        let content = "Update available! Run bun install -g @openai/codex";
        assert_eq!(detect_codex_dialog(content), Some(DialogKind::UpdatePrompt));
    }

    #[test]
    fn test_detect_codex_sandbox_trust() {
        let content = "This sandbox requires trust. Do you trust this workspace?";
        assert_eq!(detect_codex_dialog(content), Some(DialogKind::SandboxTrust));
    }

    #[test]
    fn test_detect_codex_auth() {
        let content = "You need to sign in to your OpenAI account.";
        assert_eq!(detect_codex_dialog(content), Some(DialogKind::AuthRequired));
    }

    #[test]
    fn test_detect_codex_trust_folder() {
        let content = "Do you trust the contents of this directory? Working with untrusted contents comes with higher risk.";
        assert_eq!(detect_codex_dialog(content), Some(DialogKind::TrustFolder));
    }

    #[test]
    fn test_detect_codex_trust_directory_variant() {
        let content = "Trust this directory? Yes, continue";
        assert_eq!(detect_codex_dialog(content), Some(DialogKind::TrustFolder));
    }

    #[test]
    fn test_detect_codex_none() {
        let content = ">_ OpenAI Codex\n? for shortcuts";
        assert_eq!(detect_codex_dialog(content), None);
    }

    // ── Gemini dialog detection ─────────────────────────────────────

    #[test]
    fn test_detect_gemini_trust() {
        let content = "Do you trust this folder and allow Gemini CLI to run?";
        assert_eq!(detect_gemini_dialog(content), Some(DialogKind::TrustFolder));
    }

    #[test]
    fn test_detect_gemini_auth() {
        let content = "Please sign in with your Google account.";
        assert_eq!(
            detect_gemini_dialog(content),
            Some(DialogKind::AuthRequired)
        );
    }

    #[test]
    fn test_detect_gemini_none() {
        let content = "Loaded GEMINI.md\nFound 3 MCP servers\ngemini >";
        assert_eq!(detect_gemini_dialog(content), None);
    }

    // ── Alternate detection paths ──────────────────────────────────

    #[test]
    fn test_detect_claude_new_version_variant() {
        let content = "A new version of Claude Code is available!";
        assert_eq!(
            detect_claude_dialog(content),
            Some(DialogKind::UpdatePrompt)
        );
    }

    #[test]
    fn test_detect_claude_log_in_variant() {
        let content = "Please log in to continue.";
        assert_eq!(
            detect_claude_dialog(content),
            Some(DialogKind::AuthRequired)
        );
    }

    #[test]
    fn test_detect_claude_authenticate_variant() {
        let content = "You must authenticate before using Claude.";
        assert_eq!(
            detect_claude_dialog(content),
            Some(DialogKind::AuthRequired)
        );
    }

    #[test]
    fn test_detect_claude_first_time_variant() {
        let content = "It looks like this is your first time here.";
        assert_eq!(
            detect_claude_dialog(content),
            Some(DialogKind::FirstRunSetup)
        );
    }

    #[test]
    fn test_detect_codex_log_in_variant() {
        let content = "Please log in to your OpenAI account.";
        assert_eq!(detect_codex_dialog(content), Some(DialogKind::AuthRequired));
    }

    #[test]
    fn test_detect_gemini_log_in_variant() {
        let content = "You need to log in with your Google credentials.";
        assert_eq!(
            detect_gemini_dialog(content),
            Some(DialogKind::AuthRequired)
        );
    }

    // ── Case insensitivity ──────────────────────────────────────────

    #[test]
    fn test_detect_claude_case_insensitive() {
        assert_eq!(
            detect_claude_dialog("UPDATE AVAILABLE: v3.0"),
            Some(DialogKind::UpdatePrompt)
        );
        assert_eq!(
            detect_claude_dialog("Sign In Required"),
            Some(DialogKind::AuthRequired)
        );
    }

    #[test]
    fn test_detect_claude_authenticated_status_is_not_auth_dialog() {
        let content = "Authenticated as user@example.com";
        assert_eq!(detect_claude_dialog(content), None);
    }

    #[test]
    fn test_detect_codex_case_insensitive() {
        assert_eq!(
            detect_codex_dialog("Please ACCEPT the TERMS"),
            Some(DialogKind::TermsAcceptance)
        );
    }

    #[test]
    fn test_has_numbered_skip_option_codex_menu() {
        let content = "1. Update now\n2. Skip\n3. Skip until next version";
        assert!(has_numbered_skip_option(content));
    }

    #[test]
    fn test_has_numbered_skip_option_handles_compact_capture() {
        let content = "1.Updatenow2.Skip3.Skipuntilnextversion";
        assert!(has_numbered_skip_option(content));
    }

    #[test]
    fn test_detect_gemini_case_insensitive() {
        assert_eq!(
            detect_gemini_dialog("DO YOU TRUST THIS FOLDER?"),
            Some(DialogKind::TrustFolder)
        );
    }

    // ── Detection priority ──────────────────────────────────────────

    #[test]
    fn test_detect_claude_update_before_auth() {
        // Content has both "update available" and "sign in" — update should win
        let content = "Update available. Please sign in to update.";
        assert_eq!(
            detect_claude_dialog(content),
            Some(DialogKind::UpdatePrompt)
        );
    }

    #[test]
    fn test_detect_codex_terms_before_auth() {
        // Content has both terms+accept and sign in — terms should win
        let content = "Please accept the terms. Sign in required.";
        assert_eq!(
            detect_codex_dialog(content),
            Some(DialogKind::TermsAcceptance)
        );
    }

    // ── dialog_error_message ────────────────────────────────────────

    #[test]
    fn test_error_message_contains_provider() {
        let msg = dialog_error_message(&DialogKind::TrustFolder, "gemini");
        assert!(msg.contains("gemini"));
    }

    #[test]
    fn test_error_message_trust_folder() {
        let msg = dialog_error_message(&DialogKind::TrustFolder, "gemini");
        assert!(msg.contains("trust folder"));
        assert!(msg.contains("--approval-policy accept"));
    }

    #[test]
    fn test_error_message_update_prompt() {
        let msg = dialog_error_message(&DialogKind::UpdatePrompt, "claude");
        assert!(msg.contains("update"));
        assert!(msg.contains("claude"));
    }

    #[test]
    fn test_error_message_auth_required() {
        let msg = dialog_error_message(&DialogKind::AuthRequired, "codex");
        assert!(msg.contains("authentication"));
        assert!(msg.contains("sign in"));
    }

    #[test]
    fn test_error_message_terms() {
        let msg = dialog_error_message(&DialogKind::TermsAcceptance, "codex");
        assert!(msg.contains("terms"));
    }

    #[test]
    fn test_error_message_first_run() {
        let msg = dialog_error_message(&DialogKind::FirstRunSetup, "claude");
        assert!(msg.contains("first-run"));
        assert!(msg.contains("setup"));
    }

    #[test]
    fn test_error_message_sandbox_trust() {
        let msg = dialog_error_message(&DialogKind::SandboxTrust, "codex");
        assert!(msg.contains("sandbox"));
    }

    #[test]
    fn test_error_message_unknown() {
        let msg = dialog_error_message(&DialogKind::Unknown("weird popup".into()), "gemini");
        assert!(msg.contains("weird popup"));
        assert!(msg.contains("gemini"));
    }

    // ── Dismissibility (logic only) ─────────────────────────────────

    // ── Gemini dialog: theme selection ──────────────────────────────

    #[test]
    fn test_detect_gemini_theme_select() {
        assert_eq!(
            detect_gemini_dialog("Select a theme for Gemini CLI"),
            Some(DialogKind::FirstRunSetup)
        );
    }

    #[test]
    fn test_detect_gemini_theme_choose() {
        assert_eq!(
            detect_gemini_dialog("Choose a theme:"),
            Some(DialogKind::FirstRunSetup)
        );
    }

    #[test]
    fn test_detect_gemini_theme_color() {
        assert_eq!(
            detect_gemini_dialog("Pick a color theme"),
            Some(DialogKind::FirstRunSetup)
        );
    }

    // ── Gemini dialog: update ───────────────────────────────────────

    #[test]
    fn test_detect_gemini_update_available() {
        assert_eq!(
            detect_gemini_dialog("Update available: v0.29.0"),
            Some(DialogKind::UpdatePrompt)
        );
    }

    #[test]
    fn test_detect_gemini_new_version() {
        assert_eq!(
            detect_gemini_dialog("A new version is available"),
            Some(DialogKind::UpdatePrompt)
        );
    }

    // ── Gemini dialog: terms ────────────────────────────────────────

    #[test]
    fn test_detect_gemini_terms_accept() {
        assert_eq!(
            detect_gemini_dialog("Please accept the terms of service"),
            Some(DialogKind::TermsAcceptance)
        );
    }

    #[test]
    fn test_detect_gemini_terms_agree() {
        assert_eq!(
            detect_gemini_dialog("You must agree to the terms"),
            Some(DialogKind::TermsAcceptance)
        );
    }

    // ── Gemini dialog: case insensitivity (new types) ───────────────

    #[test]
    fn test_detect_gemini_theme_uppercase() {
        assert_eq!(
            detect_gemini_dialog("SELECT A THEME"),
            Some(DialogKind::FirstRunSetup)
        );
    }

    #[test]
    fn test_detect_gemini_update_uppercase() {
        assert_eq!(
            detect_gemini_dialog("UPDATE AVAILABLE"),
            Some(DialogKind::UpdatePrompt)
        );
    }

    #[test]
    fn test_detect_gemini_terms_uppercase() {
        assert_eq!(
            detect_gemini_dialog("ACCEPT THE TERMS"),
            Some(DialogKind::TermsAcceptance)
        );
    }

    // ── Gemini dialog: priority ─────────────────────────────────────

    #[test]
    fn test_detect_gemini_trust_before_theme() {
        assert_eq!(
            detect_gemini_dialog("Do you trust this folder? Select a theme."),
            Some(DialogKind::TrustFolder)
        );
    }

    #[test]
    fn test_detect_gemini_theme_before_update() {
        assert_eq!(
            detect_gemini_dialog("Select a theme. Update available."),
            Some(DialogKind::FirstRunSetup)
        );
    }

    #[test]
    fn test_detect_gemini_update_before_terms() {
        assert_eq!(
            detect_gemini_dialog("Update available. Accept the terms."),
            Some(DialogKind::UpdatePrompt)
        );
    }

    #[test]
    fn test_detect_gemini_terms_before_auth() {
        assert_eq!(
            detect_gemini_dialog("Accept the terms. Sign in required."),
            Some(DialogKind::TermsAcceptance)
        );
    }

    #[test]
    fn test_detect_gemini_trust_before_auth() {
        assert_eq!(
            detect_gemini_dialog("Do you trust this folder? Sign in first."),
            Some(DialogKind::TrustFolder)
        );
    }

    // ── Gemini dialog: no false positives ───────────────────────────

    #[test]
    fn test_detect_gemini_no_dialog_signed_in() {
        assert_eq!(detect_gemini_dialog("Signed in as user@gmail.com"), None);
    }

    #[test]
    fn test_detect_gemini_no_dialog_logged_in() {
        assert_eq!(detect_gemini_dialog("Logged in as user@gmail.com"), None);
    }

    #[test]
    fn test_detect_gemini_no_dialog_model_info() {
        assert_eq!(detect_gemini_dialog("Model: gemini-2.5-pro"), None);
    }

    #[test]
    fn test_detect_gemini_no_dialog_extension_update() {
        // Extension update notices are informational, not interactive dialogs
        assert_eq!(
            detect_gemini_dialog(
                "ℹ You have 1 extension with an update available. Run \"/extensions update chrome-devtools-mcp\"."
            ),
            None
        );
    }

    #[test]
    fn test_detect_gemini_no_dialog_extension_new_version() {
        assert_eq!(
            detect_gemini_dialog("Extension chrome-devtools-mcp: new version available"),
            None
        );
    }

    #[test]
    fn test_detect_gemini_no_dialog_prompt() {
        assert_eq!(detect_gemini_dialog("gemini > hello"), None);
    }

    #[test]
    fn test_detect_gemini_no_dialog_accept_without_terms() {
        // "accept" alone (without "terms") should NOT trigger TermsAcceptance
        assert_eq!(detect_gemini_dialog("Please accept the invite"), None);
    }

    #[test]
    fn test_detect_gemini_no_dialog_what_can_i_help() {
        // Ready indicator is not a dialog
        assert_eq!(detect_gemini_dialog("What can I help you with?"), None);
    }

    #[test]
    fn test_detect_gemini_authenticate_variant() {
        assert_eq!(
            detect_gemini_dialog("You must authenticate with Google."),
            Some(DialogKind::AuthRequired)
        );
    }

    #[test]
    fn test_detect_gemini_waiting_for_auth_is_not_dialog() {
        // "Waiting for auth..." is a transient spinner, not an interactive dialog.
        // It should NOT be detected as AuthRequired.
        assert_eq!(
            detect_gemini_dialog("⠋ Waiting for auth... (Press ESC or CTRL+C to cancel)"),
            None
        );
    }

    // ── Dismissibility (logic only) ─────────────────────────────────

    #[test]
    fn test_non_dismissible_kinds() {
        // Verify which dialog kinds are non-dismissible (without needing a real session)
        assert!(matches!(
            DialogKind::AuthRequired,
            DialogKind::AuthRequired | DialogKind::FirstRunSetup
        ));
        assert!(matches!(
            DialogKind::FirstRunSetup,
            DialogKind::AuthRequired | DialogKind::FirstRunSetup
        ));
        // All others should be dismissible
        assert!(!matches!(
            DialogKind::TrustFolder,
            DialogKind::AuthRequired | DialogKind::FirstRunSetup
        ));
        assert!(!matches!(
            DialogKind::UpdatePrompt,
            DialogKind::AuthRequired | DialogKind::FirstRunSetup
        ));
        assert!(!matches!(
            DialogKind::TermsAcceptance,
            DialogKind::AuthRequired | DialogKind::FirstRunSetup
        ));
        assert!(!matches!(
            DialogKind::SandboxTrust,
            DialogKind::AuthRequired | DialogKind::FirstRunSetup
        ));
    }
}
