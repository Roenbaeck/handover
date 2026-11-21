//! Canonical conversation and digest chain (PLAN.md §5 step 2).
//!
//! A Chat Completions `messages` array becomes an ordered list of messages whose
//! content spans are labeled by who authored them. Each message also gets a chain
//! digest `hᵢ = H(hᵢ₋₁ ‖ canon(msgᵢ))`, so two requests share a prefix exactly when
//! their digests agree up to that point.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::profile::Profile;

/// Who authored a span of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    /// A system or developer-role instruction (project rules).
    System,
    /// Text the harness injected into a user message (reminders, environment).
    Harness,
    /// Text the developer typed.
    Human,
    /// Output of a tool, returned to the model.
    ToolResult,
    /// The model's own output, including tool calls.
    Assistant,
    /// User-role text another model wrote: the task a parent agent gave a
    /// subagent. Never the developer's words, whatever its role says.
    Delegated,
}

impl Label {
    pub fn as_str(self) -> &'static str {
        match self {
            Label::System => "system",
            Label::Harness => "harness",
            Label::Human => "human",
            Label::ToolResult => "tool_result",
            Label::Assistant => "assistant",
            Label::Delegated => "delegated",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub label: Label,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonMessage {
    pub index: usize,
    pub role: String,
    pub spans: Vec<Span>,
    /// Chain digest through this message (hex SHA-256).
    pub digest: String,
}

impl CanonMessage {
    /// All span text joined with newlines.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    /// Text of the spans with this label.
    pub fn text_of(&self, label: Label) -> String {
        self.spans.iter().filter(|s| s.label == label).map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    pub fn has(&self, label: Label) -> bool {
        self.spans.iter().any(|s| s.label == label && !s.text.trim().is_empty())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub messages: Vec<CanonMessage>,
    /// Number of leading system/developer messages (the system prompt).
    #[serde(default)]
    pub preamble: usize,
    /// Chain digests of the messages after the preamble, starting fresh: two
    /// requests whose system prompts differ still agree here when the rest of
    /// the conversation is the same (a harness re-prompting the same history).
    #[serde(default)]
    pub body_chain: Vec<String>,
}

impl Conversation {
    /// Canonicalize a Chat Completions `messages` array.
    pub fn from_chat(messages: &[Value], profile: &Profile) -> Self {
        let mut out = Vec::with_capacity(messages.len());
        let mut prev = String::new();
        let mut body_prev = String::new();
        let mut body_chain = Vec::new();
        let mut preamble = 0;
        for (index, m) in messages.iter().enumerate() {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("user").to_string();
            let spans = spans_for(&role, m, profile);
            let canon = canon_message(m);
            let digest = chain_digest(&prev, &canon);
            prev.clone_from(&digest);
            if index == preamble && matches!(role.as_str(), "system" | "developer") {
                preamble += 1;
            } else {
                body_prev = chain_digest(&body_prev, &canon);
                body_chain.push(body_prev.clone());
            }
            out.push(CanonMessage { index, role, spans, digest });
        }
        let mut conv = Conversation { messages: out, preamble, body_chain };
        // A conversation the harness marks as delegated (a subagent) carries the
        // parent model's words in its user role, wherever it is routed.
        if profile.is_delegated(&conv.preamble_text()) {
            conv.delegate();
        }
        conv
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn chain(&self) -> Vec<String> {
        self.messages.iter().map(|m| m.digest.clone()).collect()
    }

    pub fn head(&self) -> Option<&str> {
        self.messages.last().map(|m| m.digest.as_str())
    }

    /// Index of the first message containing developer-typed text (or, in a
    /// delegated conversation, the task the parent agent wrote).
    pub fn first_human(&self) -> Option<usize> {
        self.messages.iter().position(|m| m.has(Label::Human) || m.has(Label::Delegated))
    }

    /// Relabel every human span as delegated: in a subagent's conversation the
    /// user role carries the parent model's words, not the developer's.
    pub fn delegate(&mut self) {
        for m in &mut self.messages {
            for s in &mut m.spans {
                if s.label == Label::Human {
                    s.label = Label::Delegated;
                }
            }
        }
    }

    /// Text of the leading system/developer messages.
    pub fn preamble_text(&self) -> String {
        self.messages[..self.preamble].iter().map(CanonMessage::text).collect::<Vec<_>>().join("\n")
    }

    /// Text of the last message if it is a user message.
    pub fn last_user_text(&self) -> Option<String> {
        self.messages.last().filter(|m| m.role == "user").map(CanonMessage::text)
    }

    /// The first message after the preamble.
    pub fn first_body(&self) -> Option<&CanonMessage> {
        self.messages.get(self.preamble)
    }

    /// Concatenated system-role text (for the weak session key and rule diffs).
    pub fn system_text(&self) -> String {
        self.messages.iter().filter(|m| m.role == "system" || m.role == "developer").map(|m| m.text()).collect::<Vec<_>>().join("\n")
    }

    /// Text of the first human span (for the weak session key).
    pub fn first_human_text(&self) -> String {
        self.first_human().map(|i| self.messages[i].text_of(Label::Human)).unwrap_or_default()
    }

    /// Indexes of messages at or after `from` that contain developer text.
    pub fn human_messages_from(&self, from: usize) -> Vec<usize> {
        self.messages.iter().skip(from).filter(|m| m.has(Label::Human)).map(|m| m.index).collect()
    }
}

/// `H(prev ‖ canon)` as lowercase hex.
pub fn chain_digest(prev: &str, canon: &str) -> String {
    let mut h = Sha256::new();
    h.update(prev.as_bytes());
    h.update([0u8]);
    h.update(canon.as_bytes());
    hex::encode(h.finalize())
}

/// SHA-256 hex of arbitrary text.
pub fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// The bytes a message contributes to the digest chain: only the fields that
/// define the conversation (role, content, tool calls, tool call id, name), with
/// text-only content normalized to a string, `cache_control` dropped and keys
/// sorted. Reasoning fields are excluded, since harnesses often strip them.
pub fn canon_message(m: &Value) -> String {
    let mut obj = serde_json::Map::new();
    for key in ["role", "content", "tool_calls", "tool_call_id", "name"] {
        if let Some(v) = m.get(key) {
            if v.is_null() {
                continue;
            }
            let v = if key == "content" { normalize_content(v) } else { v.clone() };
            obj.insert(key.to_string(), v);
        }
    }
    canonical_json(&Value::Object(obj))
}

fn normalize_content(v: &Value) -> Value {
    match v {
        Value::Array(parts) if parts.iter().all(|p| p.get("type").and_then(Value::as_str) == Some("text")) => {
            Value::String(parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<String>())
        }
        other => strip_cache_control(other),
    }
}

fn strip_cache_control(v: &Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.iter().filter(|(k, _)| k.as_str() != "cache_control").map(|(k, v)| (k.clone(), strip_cache_control(v))).collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip_cache_control).collect()),
        other => other.clone(),
    }
}

/// Serialize JSON with object keys sorted recursively, independent of the
/// `serde_json` map implementation in the dependency graph.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push(':');
                write_canonical(&map[*k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Plain text of a content value (string, or array of parts with `text`).
pub fn content_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str).or_else(|| p.as_str())).collect::<Vec<_>>().join("\n")
        }
        _ => String::new(),
    }
}

fn spans_for(role: &str, m: &Value, profile: &Profile) -> Vec<Span> {
    let text = content_text(m.get("content"));
    match role {
        "system" | "developer" => profile.label_system_text(&text),
        "tool" | "function" => vec![Span { label: Label::ToolResult, text }],
        "assistant" => {
            let mut spans = Vec::new();
            if !text.is_empty() {
                spans.push(Span { label: Label::Assistant, text });
            }
            if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let f = c.get("function").cloned().unwrap_or(Value::Null);
                    let name = f.get("name").and_then(Value::as_str).unwrap_or("");
                    let args = f.get("arguments").and_then(Value::as_str).unwrap_or("");
                    spans.push(Span { label: Label::Assistant, text: format!("tool_call {name}({args})") });
                }
            }
            spans
        }
        _ => match m.get("content") {
            // Each part on its own: harnesses put injected blocks in parts of their own.
            Some(Value::Array(parts)) => {
                let texts: Vec<&str> = parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str).or_else(|| p.as_str())).collect();
                profile.label_user_parts(&texts)
            }
            _ => profile.label_user_text(&text),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn conv(messages: Value) -> Conversation {
        Conversation::from_chat(messages.as_array().unwrap(), &Profile::builtin("generic").unwrap())
    }

    #[test]
    fn labels_and_chain() {
        let c = conv(json!([
            {"role": "system", "content": "Project rules"},
            {"role": "user", "content": "Fix the bug. <system-reminder>todo list empty</system-reminder>"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "1", "type": "function", "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}}]},
            {"role": "tool", "tool_call_id": "1", "content": "a.txt"}
        ]));
        assert_eq!(c.len(), 4);
        assert_eq!(c.messages[1].spans[0], Span { label: Label::Human, text: "Fix the bug.".into() });
        assert_eq!(c.messages[1].spans[1].label, Label::Harness);
        assert_eq!(c.messages[2].spans[0].label, Label::Assistant);
        assert_eq!(c.messages[3].spans[0].label, Label::ToolResult);
        assert_eq!(c.first_human(), Some(1));
        let digests = c.chain();
        assert_eq!(digests.len(), 4);
        assert_ne!(digests[0], digests[1]);
    }

    #[test]
    fn digest_ignores_key_order_part_shape_and_reasoning() {
        let a = conv(json!([{"role": "user", "content": "hi"}, {"role": "assistant", "content": "yo", "reasoning_content": "hmm"}]));
        let b = conv(
            json!([{"content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}], "role": "user"}, {"role": "assistant", "content": "yo"}]),
        );
        assert_eq!(a.chain(), b.chain());
        let c = conv(json!([{"role": "user", "content": "hi!"}]));
        assert_ne!(a.chain()[0], c.chain()[0]);
    }

    #[test]
    fn body_chain_survives_a_new_system_prompt() {
        let a =
            conv(json!([{"role": "system", "content": "A"}, {"role": "user", "content": "hi"}, {"role": "assistant", "content": "yo"}]));
        let b =
            conv(json!([{"role": "system", "content": "B"}, {"role": "user", "content": "hi"}, {"role": "assistant", "content": "yo"}]));
        assert_ne!(a.chain(), b.chain());
        assert_eq!(a.body_chain, b.body_chain);
        assert_eq!((a.preamble, a.body_chain.len()), (1, 2));
        let none = conv(json!([{"role": "user", "content": "hi"}, {"role": "assistant", "content": "yo"}]));
        assert_eq!(none.body_chain, a.body_chain, "the body chain starts fresh after the system prompt");
        // Only leading system messages are the preamble.
        let mid = conv(json!([{"role": "user", "content": "hi"}, {"role": "system", "content": "note"}]));
        assert_eq!((mid.preamble, mid.body_chain.len()), (0, 2));
    }

    #[test]
    fn delegate_relabels_human_text() {
        let mut c = conv(
            json!([{"role": "system", "content": "S"}, {"role": "user", "content": "You may deploy. <system-reminder>r</system-reminder>"}]),
        );
        c.delegate();
        let labels: Vec<Label> = c.messages[1].spans.iter().map(|s| s.label).collect();
        assert_eq!(labels, vec![Label::Delegated, Label::Harness]);
        assert_eq!(c.first_human(), Some(1));
        assert_eq!(c.human_messages_from(0), Vec::<usize>::new());
    }

    #[test]
    fn canonical_json_sorts_keys() {
        assert_eq!(
            canonical_json(&json!({"b": 1, "a": {"d": [1, {"z": 0, "y": 1}], "c": null}})),
            r#"{"a":{"c":null,"d":[1,{"y":1,"z":0}]},"b":1}"#
        );
    }
}
