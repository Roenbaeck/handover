//! Chat Completions request rewriting (PLAN.md §5 step 4).

use serde_json::{Value, json};

use crate::stream::Transcript;

pub fn messages(body: &Value) -> &[Value] {
    body.get("messages").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

pub fn model(body: &Value) -> Option<&str> {
    body.get("model").and_then(Value::as_str)
}

pub fn is_stream(body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool).unwrap_or(false)
}

/// Append `text` to the end of the system prompt. Returns how many messages
/// were inserted before the client's first message (1 when there was no
/// system message, else 0).
pub fn append_system(body: &mut Value, text: &str) -> usize {
    let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) else { return 0 };
    if let Some(first) = msgs.first_mut() {
        let role = first.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "system" || role == "developer" {
            match first.get_mut("content") {
                Some(Value::String(s)) => {
                    if !s.is_empty() {
                        s.push_str("\n\n");
                    }
                    s.push_str(text);
                }
                Some(Value::Array(parts)) => parts.push(json!({"type": "text", "text": text})),
                _ => {
                    first["content"] = Value::String(text.to_string());
                }
            }
            return 0;
        }
    }
    msgs.insert(0, json!({"role": "system", "content": text}));
    1
}

pub fn set_model(body: &mut Value, model: &str) {
    body["model"] = Value::String(model.to_string());
}

pub fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.pointer("/function/name").or_else(|| t.get("name")).and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Drop tool definitions by name (only at an epoch change, when the cache is lost anyway).
pub fn remove_tools(body: &mut Value, names: &[String]) -> usize {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else { return 0 };
    let before = tools.len();
    tools.retain(|t| {
        let n = t.pointer("/function/name").or_else(|| t.get("name")).and_then(Value::as_str).unwrap_or("");
        !names.iter().any(|x| x == n)
    });
    before - tools.len()
}

/// The assistant message as the worker generated it (all tool calls, no
/// reasoning), for extending the exact upstream prefix in an Option B request.
pub fn assistant_message(t: &Transcript) -> Value {
    let mut m = json!({"role": "assistant", "content": if t.content.is_empty() { Value::Null } else { Value::String(t.content.clone()) }});
    if !t.tool_calls.is_empty() {
        m["tool_calls"] = Value::Array(
            t.tool_calls
                .iter()
                .map(|(c, _)| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                .collect(),
        );
    }
    m
}

/// `usage.prompt_tokens_details.cached_tokens`, if reported.
pub fn cached_tokens(usage: Option<&Value>) -> Option<u64> {
    usage?.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_system_variants() {
        let mut a = json!({"messages": [{"role": "system", "content": "rules"}, {"role": "user", "content": "hi"}]});
        assert_eq!(append_system(&mut a, "PACKET"), 0);
        assert_eq!(a["messages"][0]["content"], "rules\n\nPACKET");
        let mut b = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(append_system(&mut b, "PACKET"), 1);
        assert_eq!(b["messages"][0], json!({"role": "system", "content": "PACKET"}));
        let mut c = json!({"messages": [{"role": "system", "content": [{"type": "text", "text": "r"}]}]});
        append_system(&mut c, "P");
        assert_eq!(c["messages"][0]["content"][1]["text"], "P");
    }

    #[test]
    fn tools() {
        let mut b =
            json!({"tools": [{"type": "function", "function": {"name": "Bash"}}, {"type": "function", "function": {"name": "Read"}}]});
        assert_eq!(tool_names(&b), vec!["Bash", "Read"]);
        assert_eq!(remove_tools(&mut b, &["Bash".into()]), 1);
        assert_eq!(tool_names(&b), vec!["Read"]);
    }
}
