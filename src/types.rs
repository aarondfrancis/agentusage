use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ApprovalPolicy {
    /// Fail with an error when a dialog is detected
    Fail,
    /// Automatically accept/dismiss dialogs
    Accept,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DialogKind {
    TrustFolder,
    UpdatePrompt,
    AuthRequired,
    TermsAcceptance,
    FirstRunSetup,
    SandboxTrust,
    #[allow(dead_code)]
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PercentKind {
    Used,
    Left,
}

#[derive(Debug, Serialize)]
pub struct UsageEntry {
    pub label: String,
    pub percent_used: u32,
    pub percent_remaining: u32,
    #[serde(skip)]
    pub percent_kind: PercentKind,
    pub reset_info: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_minutes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UsageData {
    pub provider: String,
    pub entries: Vec<UsageEntry>,
}
