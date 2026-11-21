//! Converting between Rust values and Traqula tokens / result cells.

use chrono::{DateTime, Utc};
use handover_core::clock;
use positorium::{CollectedResultSet, ExecutionParameter};
use serde_json::Value;

/// Text of a cell, or "" when the column or row is missing.
pub fn cell<'a>(set: &'a CollectedResultSet, row: usize, col: &str) -> &'a str {
    set.cell(row, col).map(|c| c.text.as_str()).unwrap_or("")
}

/// A string literal cell (`"text"`) as plain text.
pub fn lit_str(text: &str) -> String {
    lit_str_text(text)
}

pub fn lit_str_text(text: &str) -> String {
    serde_json::from_str::<String>(text).unwrap_or_else(|_| text.to_string())
}

/// A JSON literal cell as a value.
pub fn lit_json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
}

/// `85%` → 0.85.
pub fn certainty(text: &str) -> f64 {
    text.trim().trim_end_matches('%').parse::<f64>().map(|v| v / 100.0).unwrap_or(0.0)
}

/// Normalize a time cell to `YYYY-MM-DD HH:MM:SS.ffffff`, so times compare as text.
pub fn time_text(text: &str) -> String {
    clock::parse(text).map(clock::format).unwrap_or_else(|| text.trim_matches('\'').to_string())
}

pub fn time_param(t: DateTime<Utc>) -> ExecutionParameter {
    ExecutionParameter::Time { text: format!("'{}'", clock::format(t)) }
}

/// Certainty token: integer percent in [-100, 100].
pub fn cert(c: f64) -> ExecutionParameter {
    let pct = (c.clamp(-1.0, 1.0) * 100.0).round() as i64;
    ExecutionParameter::Literal { text: format!("{pct}%") }
}

/// A JSON object literal token. Non-objects are wrapped as `{"value": ...}`.
pub fn json_param(v: &Value) -> ExecutionParameter {
    let obj = if v.is_object() { v.clone() } else { serde_json::json!({ "value": v }) };
    ExecutionParameter::Literal { text: handover_core::canonical_json(&obj) }
}
