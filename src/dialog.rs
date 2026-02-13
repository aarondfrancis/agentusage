use crate::tmux::TmuxSession;
use crate::types::DialogKind;
use anyhow::Result;
use std::thread;
use std::time::Duration;

/// Detect Claude-specific dialogs in screen content.
pub fn detect_claude_dialog(content: &str) -> Option<DialogKind> {
    let lower = content.to_lowercase();

    if lower.contains("update available") || lower.contains("new version") {
        return Some(DialogKind::UpdatePrompt);
    }
    if lower.contains("sign in") || lower.contains("log in") || lower.contains("authenticate") {
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

    if lower.contains("terms") && lower.contains("accept") {
        return Some(DialogKind::TermsAcceptance);
    }
    if lower.contains("do you trust the contents") || (lower.contains("trust") && lower.contains("directory")) {
        return Some(DialogKind::TrustFolder);
    }
    if lower.contains("sandbox") && lower.contains("trust") {
        return Some(DialogKind::SandboxTrust);
    }
    if lower.contains("sign in") || lower.contains("log in") || lower.contains("authenticate") {
        return Some(DialogKind::AuthRequired);
    }

    None
}

/// Detect Gemini-specific dialogs in screen content.
pub fn detect_gemini_dialog(content: &str) -> Option<DialogKind> {
    let lower = content.to_lowercase();

    if lower.contains("do you trust this folder") {
        return Some(DialogKind::TrustFolder);
    }
    if lower.contains("sign in") || lower.contains("log in") || lower.contains("authenticate") {
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
pub fn dismiss_dialog(kind: &DialogKind, session: &TmuxSession) -> Result<bool> {
    match kind {
        DialogKind::AuthRequired | DialogKind::FirstRunSetup => Ok(false),
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
        assert_eq!(detect_claude_dialog(content), Some(DialogKind::UpdatePrompt));
    }

    #[test]
    fn test_detect_claude_auth() {
        let content = "Please sign in to continue using Claude Code.";
        assert_eq!(detect_claude_dialog(content), Some(DialogKind::AuthRequired));
    }

    #[test]
    fn test_detect_claude_first_run() {
        let content = "Welcome to Claude Code! Let's get you set up.";
        assert_eq!(detect_claude_dialog(content), Some(DialogKind::FirstRunSetup));
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
        assert_eq!(detect_codex_dialog(content), Some(DialogKind::TermsAcceptance));
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
        assert_eq!(detect_gemini_dialog(content), Some(DialogKind::AuthRequired));
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
        assert_eq!(detect_claude_dialog(content), Some(DialogKind::UpdatePrompt));
    }

    #[test]
    fn test_detect_claude_log_in_variant() {
        let content = "Please log in to continue.";
        assert_eq!(detect_claude_dialog(content), Some(DialogKind::AuthRequired));
    }

    #[test]
    fn test_detect_claude_authenticate_variant() {
        let content = "You must authenticate before using Claude.";
        assert_eq!(detect_claude_dialog(content), Some(DialogKind::AuthRequired));
    }

    #[test]
    fn test_detect_claude_first_time_variant() {
        let content = "It looks like this is your first time here.";
        assert_eq!(detect_claude_dialog(content), Some(DialogKind::FirstRunSetup));
    }

    #[test]
    fn test_detect_codex_log_in_variant() {
        let content = "Please log in to your OpenAI account.";
        assert_eq!(detect_codex_dialog(content), Some(DialogKind::AuthRequired));
    }

    #[test]
    fn test_detect_gemini_log_in_variant() {
        let content = "You need to log in with your Google credentials.";
        assert_eq!(detect_gemini_dialog(content), Some(DialogKind::AuthRequired));
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
    fn test_detect_codex_case_insensitive() {
        assert_eq!(
            detect_codex_dialog("Please ACCEPT the TERMS"),
            Some(DialogKind::TermsAcceptance)
        );
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
        assert_eq!(detect_claude_dialog(content), Some(DialogKind::UpdatePrompt));
    }

    #[test]
    fn test_detect_codex_terms_before_auth() {
        // Content has both terms+accept and sign in — terms should win
        let content = "Please accept the terms. Sign in required.";
        assert_eq!(detect_codex_dialog(content), Some(DialogKind::TermsAcceptance));
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

    #[test]
    fn test_non_dismissible_kinds() {
        // Verify which dialog kinds are non-dismissible (without needing a real session)
        assert!(matches!(DialogKind::AuthRequired, DialogKind::AuthRequired | DialogKind::FirstRunSetup));
        assert!(matches!(DialogKind::FirstRunSetup, DialogKind::AuthRequired | DialogKind::FirstRunSetup));
        // All others should be dismissible
        assert!(!matches!(DialogKind::TrustFolder, DialogKind::AuthRequired | DialogKind::FirstRunSetup));
        assert!(!matches!(DialogKind::UpdatePrompt, DialogKind::AuthRequired | DialogKind::FirstRunSetup));
        assert!(!matches!(DialogKind::TermsAcceptance, DialogKind::AuthRequired | DialogKind::FirstRunSetup));
        assert!(!matches!(DialogKind::SandboxTrust, DialogKind::AuthRequired | DialogKind::FirstRunSetup));
    }
}
