use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PercentKind {
    Used,
    Left,
}

#[derive(Debug, Serialize)]
pub struct UsageEntry {
    pub label: String,
    pub percent: f64,
    pub percent_kind: PercentKind,
    pub reset_info: String,
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
