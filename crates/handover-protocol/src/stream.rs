//! The streaming tee and tool-call gate (PLAN.md §5 step 5, §10).
//!
//! Text and reasoning deltas pass through byte-identically as they arrive.
//! Tool-call deltas are buffered per call until the call is complete (the next
//! index starts, or `finish_reason` arrives); then the caller decides. A
//! forwarded call is replayed (re-indexed so the client sees 0..n); a replaced
//! call is dropped and its message is emitted as assistant text. If no call
//! survives, `finish_reason: tool_calls` becomes `stop`.

use std::collections::BTreeMap;

use bytes::Bytes;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::sse::{self, SseEvent};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ToolCall {
    pub index: usize,
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallDecision {
    Forward,
    /// Drop the call and say this instead.
    Replace(String),
}

/// What the tee saw, for recording and extraction.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Transcript {
    pub id: Option<String>,
    pub model: Option<String>,
    pub content: String,
    pub reasoning: String,
    /// Every complete call as generated, with whether it was forwarded.
    pub tool_calls: Vec<(ToolCall, bool)>,
    /// Upstream finish reason.
    pub finish_reason: Option<String>,
    /// Finish reason the client saw.
    pub client_finish_reason: Option<String>,
    pub usage: Option<Value>,
    pub replacements: Vec<String>,
    /// A finish reason or `[DONE]` arrived.
    pub complete: bool,
}

impl Transcript {
    /// Did this turn yield to the human (not a tool loop)?
    pub fn yields_to_human(&self) -> bool {
        self.complete && self.client_finish_reason.as_deref() != Some("tool_calls")
    }
}

#[derive(Default)]
struct Pending {
    /// Upstream `index` (or `usize::MAX` for a legacy `function_call`).
    index: usize,
    id: String,
    name: String,
    arguments: String,
    deltas: Vec<Value>,
    legacy: bool,
}

/// Index used for a legacy `delta.function_call`.
const LEGACY: usize = usize::MAX;

pub struct StreamFilter<F: FnMut(&ToolCall) -> CallDecision> {
    decide: F,
    /// Calls being buffered, keyed by an internal sequence number: two calls may
    /// share an upstream `index` (some backends reuse 0 with fresh ids).
    pending: BTreeMap<u64, Pending>,
    /// Upstream index → internal key of the call it last belonged to.
    by_index: std::collections::HashMap<usize, u64>,
    next_key: u64,
    current: Option<u64>,
    envelope: Map<String, Value>,
    forwarded: usize,
    replacements_emitted: usize,
    transcript: Transcript,
}

impl<F: FnMut(&ToolCall) -> CallDecision> StreamFilter<F> {
    pub fn new(decide: F) -> Self {
        StreamFilter {
            decide,
            pending: BTreeMap::new(),
            by_index: std::collections::HashMap::new(),
            next_key: 0,
            current: None,
            envelope: Map::new(),
            forwarded: 0,
            replacements_emitted: 0,
            transcript: Transcript::default(),
        }
    }

    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    pub fn into_transcript(self) -> Transcript {
        self.transcript
    }

    /// Process one upstream event; returns the bytes to send to the client.
    pub fn event(&mut self, ev: &SseEvent) -> Vec<Bytes> {
        if ev.data.trim() == "[DONE]" {
            let mut out = self.flush_all();
            out.extend(self.emit_replacements());
            self.transcript.complete = true;
            out.push(ev.raw.clone());
            return out;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(&ev.data) else {
            return vec![ev.raw.clone()];
        };
        if let Some(obj) = chunk.as_object() {
            for (k, v) in obj {
                if k != "choices" && k != "usage" {
                    self.envelope.insert(k.clone(), v.clone());
                }
            }
        }
        if let Some(m) = chunk.get("model").and_then(Value::as_str) {
            self.transcript.model = Some(m.to_string());
        }
        if let Some(id) = chunk.get("id").and_then(Value::as_str) {
            self.transcript.id.get_or_insert_with(|| id.to_string());
        }
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.transcript.usage = Some(u.clone());
        }
        // Several choices: keep only the one every translator reads (index 0), so
        // nothing reaches a client that the gate did not see.
        let mut chunk = chunk;
        let mut raw = ev.raw.clone();
        if let Some(choices) = chunk.get("choices").and_then(Value::as_array).filter(|c| c.len() > 1) {
            let chosen = choices.iter().find(|c| c.get("index").and_then(Value::as_u64) == Some(0)).unwrap_or(&choices[0]).clone();
            chunk["choices"] = json!([chosen]);
            raw = sse::data(&chunk);
        }
        let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else {
            return vec![raw];
        };
        let delta = choice.get("delta").cloned().unwrap_or(Value::Object(Map::new()));
        let finish = choice.get("finish_reason").and_then(Value::as_str).map(str::to_string);
        let mut calls = delta.get("tool_calls").and_then(Value::as_array).cloned();
        // A legacy `function_call` delta is gated like a tool call.
        if let Some(fc) = delta.get("function_call").filter(|f| f.is_object()) {
            calls.get_or_insert_with(Vec::new).push(json!({"index": LEGACY, "legacy": fc}));
        }
        self.tee_text(&delta);

        if calls.is_none() && finish.is_none() {
            return vec![raw];
        }
        if calls.is_none() && self.pending.is_empty() && self.transcript.replacements.is_empty() {
            // A plain finish chunk with nothing buffered: forward byte-identically.
            self.transcript.finish_reason = finish.clone();
            self.transcript.client_finish_reason = finish;
            self.transcript.complete = true;
            return vec![raw];
        }

        let mut out = Vec::new();
        let mut rest = delta.as_object().cloned().unwrap_or_default();
        rest.remove("tool_calls");
        rest.remove("function_call");
        let has_text = ["content", "reasoning_content", "reasoning"]
            .iter()
            .any(|k| rest.get(*k).and_then(Value::as_str).is_some_and(|s| !s.is_empty()));
        if has_text {
            out.push(self.chunk(Value::Object(rest), Value::Null, None));
        }
        for tc in calls.unwrap_or_default() {
            if !tc.is_object() {
                continue; // malformed entry: nothing to gate or replay
            }
            let legacy = tc.get("legacy").cloned();
            let idx = tc.get("index").and_then(Value::as_u64).map(|i| i as usize).unwrap_or(0);
            let id = tc.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
            let func = legacy.clone().or_else(|| tc.get("function").cloned()).unwrap_or(Value::Null);
            let key = match self.by_index.get(&idx).copied() {
                // Same index, different id: a new call (complete the previous one first).
                Some(k) if self.pending.get(&k).is_some_and(|p| id.as_ref().is_some_and(|id| !p.id.is_empty() && *id != p.id)) => {
                    out.extend(self.complete(k));
                    None
                }
                Some(k) if self.pending.contains_key(&k) => Some(k),
                // A fragment for a call that was already decided: drop it, never
                // let it start a nameless call of its own.
                Some(_) if id.is_none() && func.get("name").is_none() => continue,
                _ => None,
            };
            let key = key.unwrap_or_else(|| {
                let k = self.next_key;
                self.next_key += 1;
                self.by_index.insert(idx, k);
                self.pending.insert(k, Pending { index: idx, legacy: legacy.is_some(), ..Default::default() });
                k
            });
            if let Some(cur) = self.current
                && cur != key
            {
                out.extend(self.complete(cur));
            }
            self.current = Some(key);
            let p = self.pending.get_mut(&key).expect("pending call");
            if let Some(id) = id {
                p.id = id;
            }
            if let Some(n) = func.get("name").and_then(Value::as_str) {
                p.name.push_str(n);
            }
            // Arguments as a JSON object (some servers): gate and forward the same string.
            let mut stored = legacy.unwrap_or(tc);
            match func.get("arguments") {
                Some(Value::String(a)) => p.arguments.push_str(a),
                Some(obj @ (Value::Object(_) | Value::Array(_))) => {
                    let text = obj.to_string();
                    p.arguments.push_str(&text);
                    if p.legacy {
                        stored["arguments"] = Value::String(text);
                    } else if stored.get("function").is_some_and(Value::is_object) {
                        stored["function"]["arguments"] = Value::String(text);
                    }
                }
                _ => {}
            }
            p.deltas.push(stored);
        }
        if let Some(reason) = finish {
            out.extend(self.flush_all());
            out.extend(self.emit_replacements());
            let client =
                if (reason == "tool_calls" || reason == "function_call") && self.forwarded == 0 && !self.transcript.replacements.is_empty()
                {
                    "stop".to_string()
                } else {
                    reason.clone()
                };
            let usage = chunk.get("usage").cloned().unwrap_or(Value::Null);
            out.push(self.chunk(json!({}), usage, Some(&client)));
            self.transcript.finish_reason = Some(reason);
            self.transcript.client_finish_reason = Some(client);
            self.transcript.complete = true;
        }
        out
    }

    /// End of the upstream body. A call still buffered here is truncated: it is
    /// dropped, never forwarded half-written.
    pub fn finish(&mut self) -> Vec<Bytes> {
        for (_, p) in std::mem::take(&mut self.pending) {
            self.transcript.tool_calls.push((ToolCall { index: p.index, id: p.id, name: p.name, arguments: p.arguments }, false));
        }
        Vec::new()
    }

    fn tee_text(&mut self, delta: &Value) {
        if let Some(s) = delta.get("content").and_then(Value::as_str) {
            self.transcript.content.push_str(s);
        }
        if let Some(s) = delta.get("reasoning_content").or_else(|| delta.get("reasoning")).and_then(Value::as_str) {
            self.transcript.reasoning.push_str(s);
        }
    }

    fn chunk(&self, delta: Value, usage: Value, finish: Option<&str>) -> Bytes {
        let mut c = self.envelope.clone();
        c.insert("choices".into(), json!([{"index": 0, "delta": delta, "logprobs": null, "finish_reason": finish}]));
        if !usage.is_null() {
            c.insert("usage".into(), usage);
        }
        sse::data(&Value::Object(c))
    }

    fn complete(&mut self, key: u64) -> Vec<Bytes> {
        let Some(p) = self.pending.remove(&key) else { return Vec::new() };
        if self.current == Some(key) {
            self.current = None;
        }
        let call = ToolCall { index: p.index, id: p.id, name: p.name, arguments: p.arguments };
        match (self.decide)(&call) {
            CallDecision::Forward => {
                let out = if p.legacy {
                    p.deltas.into_iter().map(|fc| self.chunk(json!({"function_call": fc}), Value::Null, None)).collect()
                } else {
                    let new_index = self.forwarded;
                    self.forwarded += 1;
                    p.deltas
                        .into_iter()
                        .map(|mut tc| {
                            tc["index"] = json!(new_index);
                            self.chunk(json!({"tool_calls": [tc]}), Value::Null, None)
                        })
                        .collect()
                };
                if p.legacy {
                    self.forwarded += 1;
                }
                self.transcript.tool_calls.push((call, true));
                out
            }
            CallDecision::Replace(msg) => {
                self.transcript.replacements.push(msg);
                self.transcript.tool_calls.push((call, false));
                Vec::new()
            }
        }
    }

    fn flush_all(&mut self) -> Vec<Bytes> {
        let keys: Vec<u64> = self.pending.keys().copied().collect();
        keys.into_iter().flat_map(|k| self.complete(k)).collect()
    }

    fn emit_replacements(&mut self) -> Vec<Bytes> {
        let new: Vec<String> = self.transcript.replacements[self.replacements_emitted..].to_vec();
        self.replacements_emitted = self.transcript.replacements.len();
        if new.is_empty() {
            return Vec::new();
        }
        let sep = if self.transcript.content.is_empty() { "" } else { "\n\n" };
        vec![self.chunk(json!({"content": format!("{sep}{}", new.join("\n\n"))}), Value::Null, None)]
    }
}

/// Apply the same gate to a non-streaming `chat.completion` body in place.
pub fn filter_completion<F: FnMut(&ToolCall) -> CallDecision>(resp: &mut Value, mut decide: F) -> Transcript {
    let mut t = Transcript {
        id: resp.get("id").and_then(Value::as_str).map(str::to_string),
        model: resp.get("model").and_then(Value::as_str).map(str::to_string),
        usage: resp.get("usage").cloned(),
        complete: true,
        ..Default::default()
    };
    if let Some(choices) = resp.get_mut("choices").and_then(Value::as_array_mut).filter(|c| c.len() > 1) {
        let pick = choices.iter().position(|c| c.get("index").and_then(Value::as_u64) == Some(0)).unwrap_or(0);
        let chosen = choices.swap_remove(pick);
        *choices = vec![chosen];
    }
    let Some(choice) = resp.get_mut("choices").and_then(Value::as_array_mut).and_then(|c| c.first_mut()).filter(|c| c.is_object()) else {
        return t;
    };
    let finish = choice.get("finish_reason").and_then(Value::as_str).map(str::to_string);
    if !choice.get("message").is_some_and(Value::is_object) {
        return t;
    }
    let msg = &mut choice["message"];
    t.content = msg.get("content").and_then(Value::as_str).unwrap_or("").to_string();
    t.reasoning = msg.get("reasoning_content").and_then(Value::as_str).unwrap_or("").to_string();
    let mut kept = Vec::new();
    let mut legacy_kept = false;
    // A legacy `function_call` is gated like a tool call.
    if let Some(fc) = msg.get("function_call").filter(|f| f.is_object()).cloned() {
        let call = ToolCall {
            index: LEGACY,
            id: String::new(),
            name: fc.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            arguments: fc.get("arguments").and_then(Value::as_str).unwrap_or("").to_string(),
        };
        match decide(&call) {
            CallDecision::Forward => {
                legacy_kept = true;
                t.tool_calls.push((call, true));
            }
            CallDecision::Replace(m) => {
                msg.as_object_mut().map(|o| o.remove("function_call"));
                let sep = if t.content.is_empty() { "" } else { "\n\n" };
                msg["content"] = Value::String(format!("{}{sep}{m}", t.content));
                t.replacements.push(m);
                t.tool_calls.push((call, false));
            }
        }
    }
    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array).cloned() {
        let mut normalized = false;
        let calls: Vec<Value> = calls
            .into_iter()
            .filter(Value::is_object)
            .map(|mut c| {
                if let Some(obj) = c.pointer("/function/arguments").filter(|a| a.is_object() || a.is_array()).cloned() {
                    c["function"]["arguments"] = Value::String(obj.to_string());
                    normalized = true;
                }
                c
            })
            .collect();
        if normalized {
            msg["tool_calls"] = Value::Array(calls.clone());
        }
        for (i, c) in calls.into_iter().enumerate() {
            let call = ToolCall {
                index: i,
                id: c.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                name: c.pointer("/function/name").and_then(Value::as_str).unwrap_or("").to_string(),
                arguments: c.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("").to_string(),
            };
            match decide(&call) {
                CallDecision::Forward => {
                    kept.push(c);
                    t.tool_calls.push((call, true));
                }
                CallDecision::Replace(m) => {
                    t.replacements.push(m);
                    t.tool_calls.push((call, false));
                }
            }
        }
        if t.tool_calls.iter().any(|(_, f)| !f) {
            if kept.is_empty() {
                msg.as_object_mut().map(|o| o.remove("tool_calls"));
            } else {
                msg["tool_calls"] = Value::Array(kept.clone());
            }
            let sep = if t.content.is_empty() { "" } else { "\n\n" };
            msg["content"] = Value::String(format!("{}{sep}{}", t.content, t.replacements.join("\n\n")));
        }
    }
    let client = match finish.as_deref() {
        Some("tool_calls" | "function_call") if kept.is_empty() && !legacy_kept && !t.replacements.is_empty() => Some("stop".to_string()),
        other => other.map(str::to_string),
    };
    if client != finish {
        choice["finish_reason"] = json!(client);
    }
    t.finish_reason = finish;
    t.client_finish_reason = client;
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseParser;

    fn upstream() -> Vec<u8> {
        let chunks = [
            json!({"id": "c1", "object": "chat.completion.chunk", "model": "m", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "model": "m", "choices": [{"index": 0, "delta": {"content": "Let me check."}, "finish_reason": null}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "model": "m", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "a", "type": "function", "function": {"name": "Bash", "arguments": ""}}]}, "finish_reason": null}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "model": "m", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"command\":\"make deploy-prod\"}"}}]}, "finish_reason": null}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "model": "m", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 1, "id": "b", "type": "function", "function": {"name": "Read", "arguments": "{\"file_path\":\"a\"}"}}]}, "finish_reason": null}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "model": "m", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ];
        let mut out = Vec::new();
        for c in chunks {
            out.extend_from_slice(format!("data: {c}\n\n").as_bytes());
        }
        out.extend_from_slice(b"data: [DONE]\n\n");
        out
    }

    fn run(decide: impl FnMut(&ToolCall) -> CallDecision) -> (Vec<Value>, Transcript, Vec<u8>) {
        let mut parser = SseParser::new();
        let mut f = StreamFilter::new(decide);
        let mut bytes = Vec::new();
        for ev in parser.push(&upstream()) {
            for b in f.event(&ev) {
                bytes.extend_from_slice(&b);
            }
        }
        f.finish();
        let mut p2 = SseParser::new();
        let values = p2.push(&bytes).into_iter().filter(|e| e.data != "[DONE]").map(|e| serde_json::from_str(&e.data).unwrap()).collect();
        (values, f.into_transcript(), bytes)
    }

    fn reassemble(values: &[Value]) -> (String, Vec<(u64, String, String)>, Option<String>) {
        let mut text = String::new();
        let mut calls: BTreeMap<u64, (String, String)> = BTreeMap::new();
        let mut finish = None;
        for v in values {
            let c = &v["choices"][0];
            if let Some(s) = c["delta"]["content"].as_str() {
                text.push_str(s);
            }
            for tc in c["delta"]["tool_calls"].as_array().cloned().unwrap_or_default() {
                let e = calls.entry(tc["index"].as_u64().unwrap()).or_default();
                e.0.push_str(tc["function"]["name"].as_str().unwrap_or(""));
                e.1.push_str(tc["function"]["arguments"].as_str().unwrap_or(""));
            }
            if let Some(f) = c["finish_reason"].as_str() {
                finish = Some(f.to_string());
            }
        }
        (text, calls.into_iter().map(|(i, (n, a))| (i, n, a)).collect(), finish)
    }

    #[test]
    fn forward_all_is_semantically_identical_and_text_is_byte_identical() {
        let (values, t, bytes) = run(|_| CallDecision::Forward);
        let (text, calls, finish) = reassemble(&values);
        assert_eq!(text, "Let me check.");
        assert_eq!(calls.len(), 2);
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        assert!(t.complete && !t.yields_to_human());
        let original = upstream();
        // The first two (text) events are forwarded byte for byte.
        let head = String::from_utf8_lossy(&original).split("\n\n").take(2).collect::<Vec<_>>().join("\n\n");
        assert!(String::from_utf8_lossy(&bytes).starts_with(&head));
    }

    #[test]
    fn blocking_one_call_reindexes_the_rest() {
        let (values, t, _) = run(|c| if c.name == "Bash" { CallDecision::Replace("Blocked by P1.".into()) } else { CallDecision::Forward });
        let (text, calls, finish) = reassemble(&values);
        assert_eq!(calls, vec![(0, "Read".to_string(), "{\"file_path\":\"a\"}".to_string())]);
        assert_eq!(text, "Let me check.\n\nBlocked by P1.");
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        assert_eq!(t.tool_calls.len(), 2);
        assert_eq!(t.tool_calls[0].0.arguments, "{\"command\":\"make deploy-prod\"}");
        assert!(!t.tool_calls[0].1);
    }

    #[test]
    fn blocking_every_call_finishes_with_stop() {
        let (values, t, _) = run(|_| CallDecision::Replace("no".into()));
        let (_, calls, finish) = reassemble(&values);
        assert!(calls.is_empty());
        assert_eq!(finish.as_deref(), Some("stop"));
        assert!(t.yields_to_human());
    }

    #[test]
    fn non_streaming() {
        let mut resp = json!({"id": "x", "model": "m", "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {"role": "assistant", "content": null, "tool_calls": [{"id": "a", "type": "function", "function": {"name": "Bash", "arguments": "{}"}}]}}]});
        let t = filter_completion(&mut resp, |_| CallDecision::Replace("Blocked.".into()));
        assert_eq!(resp["choices"][0]["finish_reason"], "stop");
        assert_eq!(resp["choices"][0]["message"]["content"], "Blocked.");
        assert!(resp["choices"][0]["message"].get("tool_calls").is_none());
        assert_eq!(t.client_finish_reason.as_deref(), Some("stop"));
    }

    fn run_chunks(chunks: &[Value], decide: impl FnMut(&ToolCall) -> CallDecision) -> (Vec<Value>, Transcript) {
        let mut raw = Vec::new();
        for c in chunks {
            raw.extend_from_slice(format!("data: {c}\n\n").as_bytes());
        }
        raw.extend_from_slice(b"data: [DONE]\n\n");
        let mut parser = SseParser::new();
        let mut f = StreamFilter::new(decide);
        let mut bytes = Vec::new();
        for ev in parser.push(&raw) {
            for b in f.event(&ev) {
                bytes.extend_from_slice(&b);
            }
        }
        f.finish();
        let values = SseParser::new()
            .push(&bytes)
            .into_iter()
            .filter(|e| e.data != "[DONE]")
            .map(|e| serde_json::from_str(&e.data).unwrap())
            .collect();
        (values, f.into_transcript())
    }

    fn chunk(delta: Value, finish: Option<&str>) -> Value {
        json!({"id": "c", "object": "chat.completion.chunk", "model": "m", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    }

    /// Review finding: two calls sharing index 0 (fresh ids) must be gated separately.
    #[test]
    fn calls_sharing_an_index_are_gated_separately() {
        let chunks = [
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "a", "type": "function", "function": {"name": "Read", "arguments": "{}"}}]}),
                None,
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "b", "type": "function", "function": {"name": "Bash", "arguments": ""}}]}),
                None,
            ),
            chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"command\":\"make deploy-prod\"}"}}]}), None),
            chunk(
                json!({"tool_calls": [{"id": "c", "type": "function", "function": {"name": "Bash", "arguments": "{\"command\":\"make deploy-prod\"}"}}]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ];
        let (values, t) =
            run_chunks(&chunks, |c| if c.name == "Bash" { CallDecision::Replace("Blocked.".into()) } else { CallDecision::Forward });
        let names: Vec<&str> = t.tool_calls.iter().map(|(c, _)| c.name.as_str()).collect();
        assert_eq!(names, vec!["Read", "Bash", "Bash"]);
        let (_, calls, finish) = reassemble(&values);
        assert_eq!(calls, vec![(0, "Read".to_string(), "{}".to_string())], "only Read reaches the client");
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        // No forwarded chunk carries a Bash call.
        assert!(values.iter().all(|v| !v.to_string().contains("deploy-prod")));
    }

    #[test]
    fn malformed_entries_do_not_panic_and_legacy_function_calls_are_gated() {
        let chunks = [
            chunk(json!({"tool_calls": [5, "x", null]}), None),
            chunk(json!({"function_call": {"name": "Bash", "arguments": ""}}), None),
            chunk(json!({"function_call": {"arguments": "{\"command\":\"rm -rf /\"}"}}), None),
            chunk(json!({}), Some("function_call")),
        ];
        let (values, t) = run_chunks(&chunks, |c| {
            assert_eq!((c.name.as_str(), c.arguments.as_str()), ("Bash", "{\"command\":\"rm -rf /\"}"));
            CallDecision::Replace("Blocked.".into())
        });
        assert!(values.iter().all(|v| v["choices"][0]["delta"].get("function_call").is_none()));
        assert_eq!(t.client_finish_reason.as_deref(), Some("stop"));
        let mut resp = json!({"choices": [{"finish_reason": "function_call", "message": {"role": "assistant", "content": null, "function_call": {"name": "Bash", "arguments": "{}"}}}]});
        filter_completion(&mut resp, |_| CallDecision::Replace("Blocked.".into()));
        assert!(resp["choices"][0]["message"].get("function_call").is_none());
        assert_eq!(resp["choices"][0]["finish_reason"], "stop");
        let mut odd = json!({"choices": [7]});
        filter_completion(&mut odd, |_| CallDecision::Forward);
        let mut odd = json!({"choices": [{"message": 7}]});
        filter_completion(&mut odd, |_| CallDecision::Forward);
    }

    /// Review finding: object arguments and extra choices must not bypass the gate.
    #[test]
    fn object_arguments_and_extra_choices_are_gated() {
        let block = |c: &ToolCall| {
            if c.arguments.contains("deploy-prod") { CallDecision::Replace("Blocked.".into()) } else { CallDecision::Forward }
        };
        let mut resp = json!({"choices": [{"index": 0, "finish_reason": "tool_calls", "message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": "a", "type": "function", "function": {"name": "Bash", "arguments": {"command": "make deploy-prod"}}}]}}]});
        filter_completion(&mut resp, block);
        assert!(resp["choices"][0]["message"].get("tool_calls").is_none(), "{resp}");
        let chunks = [
            json!({"id": "c", "model": "m", "choices": [
                {"index": 1, "delta": {"content": "hi"}, "finish_reason": null},
                {"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "a", "type": "function", "function": {"name": "Bash", "arguments": {"command": "make deploy-prod"}}}]}, "finish_reason": null}
            ]}),
            json!({"id": "c", "model": "m", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ];
        let (values, t) = run_chunks(&chunks, block);
        assert_eq!(t.tool_calls.len(), 1);
        assert!(!t.tool_calls[0].1, "blocked");
        assert!(values.iter().all(|v| v["choices"].as_array().unwrap().len() == 1));
        assert!(values.iter().all(|v| !v.to_string().contains("deploy-prod")));
    }
}
