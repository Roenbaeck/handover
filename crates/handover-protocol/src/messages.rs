//! Anthropic Messages adapter (PLAN.md §13).
//!
//! Claude Code speaks Anthropic Messages. When the next hop only understands
//! Chat Completions, the proxy translates the request with [`request_to_chat`]
//! and the response back with [`completion_to_message`] (non-streaming) or a
//! [`StreamTranslator`] (streaming). [`session_from_metadata`] extracts Claude
//! Code's native session key.
//!
//! Every function here is total: malformed input yields an `Err` (only where a
//! request cannot be translated at all) or is skipped, never a panic.

use bytes::Bytes;
use serde_json::{Map, Value, json};

use crate::sse;

// ---------------------------------------------------------------------------
// Request: Anthropic Messages -> Chat Completions
// ---------------------------------------------------------------------------

/// Anthropic Messages request body → OpenAI Chat Completions request body.
///
/// Fails only when `messages` is missing or not an array. Unknown block types,
/// unknown roles and server tools (tools with a non-`custom` `type`) are
/// skipped.
pub fn request_to_chat(req: &Value) -> Result<Value, String> {
    let messages = req.get("messages").ok_or("request has no `messages`")?.as_array().ok_or("`messages` is not an array")?;

    let mut out = Map::new();
    for key in ["model", "max_tokens", "temperature", "top_p"] {
        if let Some(v) = req.get(key).filter(|v| !v.is_null()) {
            out.insert(key.to_string(), v.clone());
        }
    }
    if let Some(stop) = req.get("stop_sequences").filter(|v| v.is_array()) {
        out.insert("stop".into(), stop.clone());
    }

    let mut chat_messages = Vec::new();
    if let Some(system) = req.get("system").map(text_of).filter(|s| !s.is_empty()) {
        chat_messages.push(json!({"role": "system", "content": system}));
    }
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("user") => user_message(message.get("content"), &mut chat_messages),
            Some("assistant") => chat_messages.push(assistant_message(message.get("content"))),
            _ => {}
        }
    }
    out.insert("messages".into(), Value::Array(chat_messages));

    let tools: Vec<Value> =
        req.get("tools").and_then(Value::as_array).map(|tools| tools.iter().filter_map(tool).collect()).unwrap_or_default();
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = req.get("tool_choice") {
            if let Some(mapped) = tool_choice(choice) {
                out.insert("tool_choice".into(), mapped);
            }
            if choice.get("disable_parallel_tool_use").and_then(Value::as_bool) == Some(true) {
                out.insert("parallel_tool_calls".into(), Value::Bool(false));
            }
        }
    }

    if req.get("stream").and_then(Value::as_bool) == Some(true) {
        out.insert("stream".into(), Value::Bool(true));
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Ok(Value::Object(out))
}

/// Text of a string, or of the `text` blocks of a block array, joined with a
/// blank line. Anything else yields an empty string.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| block_type(b) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

/// A user turn: `tool_result` blocks become `tool` messages first, then the
/// remaining text (and images) become one user message.
fn user_message(content: Option<&Value>, out: &mut Vec<Value>) {
    let blocks = match content {
        Some(Value::String(s)) => {
            out.push(json!({"role": "user", "content": s}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => return,
    };

    let mut parts = Vec::new(); // text and image parts, in order
    let mut has_image = false;
    for block in blocks {
        match block_type(block) {
            Some("tool_result") => {
                let id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or_default();
                let mut text = block.get("content").map(text_of).unwrap_or_default();
                if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                    text = format!("Error: {text}");
                }
                out.push(json!({"role": "tool", "tool_call_id": id, "content": text}));
            }
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(json!({"type": "text", "text": text}));
                }
            }
            Some("image") => {
                if let Some(part) = image_part(block) {
                    has_image = true;
                    parts.push(part);
                }
            }
            _ => {}
        }
    }

    // Several text blocks stay separate parts: harnesses put injected text
    // (reminders) in blocks of their own, and labeling runs per part.
    if has_image || parts.len() > 1 {
        out.push(json!({"role": "user", "content": parts}));
    } else if let Some(text) = parts.first().and_then(|p| p.get("text")).and_then(Value::as_str) {
        out.push(json!({"role": "user", "content": text}));
    }
}

fn image_part(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    let media_type = source.get("media_type").and_then(Value::as_str)?;
    let data = source.get("data").and_then(Value::as_str)?;
    Some(json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{media_type};base64,{data}")}
    }))
}

/// An assistant turn: text → `content`, `tool_use` → `tool_calls`, thinking
/// dropped.
fn assistant_message(content: Option<&Value>) -> Value {
    let mut texts = Vec::new();
    let mut tool_calls = Vec::new();
    match content {
        Some(Value::String(s)) => texts.push(s.as_str()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block_type(block) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            texts.push(text);
                        }
                    }
                    Some("tool_use") => {
                        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                        let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
                        tool_calls.push(json!({
                            "id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
                            "type": "function",
                            "function": {
                                "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
                                "arguments": arguments,
                            }
                        }));
                    }
                    _ => {} // thinking, redacted_thinking, unknown
                }
            }
        }
        _ => {}
    }

    let mut message = Map::new();
    message.insert("role".into(), "assistant".into());
    let content = if !texts.is_empty() {
        Value::String(texts.join("\n\n"))
    } else if tool_calls.is_empty() {
        // Chat Completions requires content when there are no tool calls.
        Value::String(String::new())
    } else {
        Value::Null
    };
    message.insert("content".into(), content);
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    Value::Object(message)
}

fn tool(tool: &Value) -> Option<Value> {
    // Server tools (web_search_…, bash_…) have a typed `type` and no schema;
    // Chat Completions cannot express them.
    if let Some(t) = tool.get("type").and_then(Value::as_str)
        && t != "custom"
    {
        return None;
    }
    let name = tool.get("name").and_then(Value::as_str)?;
    let mut function = Map::new();
    function.insert("name".into(), name.into());
    if let Some(description) = tool.get("description").and_then(Value::as_str) {
        function.insert("description".into(), description.into());
    }
    let parameters =
        tool.get("input_schema").filter(|s| s.is_object()).cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    function.insert("parameters".into(), parameters);
    Some(json!({"type": "function", "function": function}))
}

fn tool_choice(choice: &Value) -> Option<Value> {
    match choice.get("type").and_then(Value::as_str)? {
        "auto" => Some("auto".into()),
        "any" => Some("required".into()),
        "none" => Some("none".into()),
        "tool" => {
            let name = choice.get("name").and_then(Value::as_str)?;
            Some(json!({"type": "function", "function": {"name": name}}))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Response: Chat Completions -> Anthropic Messages
// ---------------------------------------------------------------------------

fn stop_reason(finish_reason: Option<&str>) -> Value {
    match finish_reason {
        None => Value::Null,
        Some("length") => "max_tokens".into(),
        Some("tool_calls" | "function_call") => "tool_use".into(),
        Some(_) => "end_turn".into(), // stop, content_filter, anything else
    }
}

fn u64_at(v: &Value, pointer: &str) -> u64 {
    v.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

/// Anthropic counts cache reads separately from `input_tokens`, while
/// Chat Completions' `prompt_tokens` includes them.
fn usage_counts(usage: &Value) -> (u64, u64, u64) {
    let prompt = u64_at(usage, "/prompt_tokens");
    let cached = u64_at(usage, "/prompt_tokens_details/cached_tokens");
    let completion = u64_at(usage, "/completion_tokens");
    (prompt.saturating_sub(cached), completion, cached)
}

/// Tool-call arguments as an input object: parsed JSON when it is an object,
/// `{}` otherwise.
fn parse_arguments(arguments: Option<&Value>) -> Value {
    let parsed = match arguments {
        Some(Value::String(s)) => serde_json::from_str(s).ok(),
        Some(v @ Value::Object(_)) => Some(v.clone()),
        _ => None,
    };
    parsed.filter(Value::is_object).unwrap_or_else(|| json!({}))
}

/// Non-streaming Chat Completions response → Anthropic message object.
pub fn completion_to_message(chat: &Value) -> Value {
    let id = chat.get("id").and_then(Value::as_str).unwrap_or_default();
    let choice = chat.pointer("/choices/0");
    let message = choice.and_then(|c| c.get("message"));

    let mut content = Vec::new();
    if let Some(text) = message.and_then(|m| m.get("content")).and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(json!({"type": "text", "text": text}));
    }
    let tool_calls = message.and_then(|m| m.get("tool_calls")).and_then(Value::as_array);
    for call in tool_calls.into_iter().flatten() {
        let function = call.get("function");
        content.push(json!({
            "type": "tool_use",
            "id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
            "name": function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or_default(),
            "input": parse_arguments(function.and_then(|f| f.get("arguments"))),
        }));
    }

    let finish = choice.and_then(|c| c.get("finish_reason")).and_then(Value::as_str);
    let (input, output, cached) = usage_counts(chat.get("usage").unwrap_or(&Value::Null));
    json!({
        "id": format!("msg_{id}"),
        "type": "message",
        "role": "assistant",
        "model": chat.get("model").cloned().unwrap_or(Value::Null),
        "content": content,
        "stop_reason": stop_reason(finish),
        "stop_sequence": null,
        "usage": {
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_input_tokens": cached,
        }
    })
}

// ---------------------------------------------------------------------------
// Streaming: Chat Completions chunks -> Anthropic SSE events
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum OpenBlock {
    Text { index: u64 },
    Tool { index: u64, call: u64 },
}

impl OpenBlock {
    fn index(&self) -> u64 {
        match self {
            OpenBlock::Text { index } | OpenBlock::Tool { index, .. } => *index,
        }
    }
}

/// A tool call seen in the stream, keyed by its Chat Completions `index`.
#[derive(Debug)]
struct SeenCall {
    call: u64,
    id: String,
}

/// Streaming translation of Chat Completions chunks into Anthropic SSE events.
///
/// Feed every parsed `chat.completion.chunk` to [`chunk`](Self::chunk) and call
/// [`finish`](Self::finish) once at the end of the stream. Argument fragments
/// for a tool call whose block was already closed (interleaved parallel calls)
/// are dropped, since Anthropic blocks cannot be reopened.
#[derive(Debug)]
pub struct StreamTranslator {
    model: String,
    started: bool,
    stopped: bool,
    next_index: u64,
    open: Option<OpenBlock>,
    calls: Vec<SeenCall>,
    next_call: u64,
    stop_reason: Value,
    usage: Option<Value>,
}

impl StreamTranslator {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            started: false,
            stopped: false,
            next_index: 0,
            open: None,
            calls: Vec::new(),
            next_call: 0,
            stop_reason: Value::Null,
            usage: None,
        }
    }

    /// One parsed `chat.completion.chunk` JSON value → zero or more Anthropic
    /// SSE events.
    pub fn chunk(&mut self, chunk: &Value) -> Vec<Bytes> {
        let mut out = Vec::new();
        if self.stopped {
            return out;
        }
        self.start(chunk.get("id").and_then(Value::as_str), &mut out);

        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage.clone());
        }

        let choices = chunk.get("choices").and_then(Value::as_array);
        let choice = choices.and_then(|cs| cs.iter().find(|c| c.get("index").and_then(Value::as_u64) == Some(0)).or_else(|| cs.first()));
        let Some(choice) = choice else { return out };

        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                self.text(text, &mut out);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.tool_call(call, &mut out);
                }
            }
        }

        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = stop_reason(Some(finish));
            self.close(&mut out);
        }
        out
    }

    /// Call once at end of stream (after `[DONE]` or EOF): closes any open
    /// block and emits `message_delta` + `message_stop` if not yet emitted.
    pub fn finish(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        if self.stopped {
            return out;
        }
        self.start(None, &mut out);
        self.close(&mut out);

        let mut usage = Map::new();
        match &self.usage {
            Some(u) => {
                let (input, output, cached) = usage_counts(u);
                usage.insert("output_tokens".into(), output.into());
                usage.insert("input_tokens".into(), input.into());
                usage.insert("cache_read_input_tokens".into(), cached.into());
            }
            None => {
                usage.insert("output_tokens".into(), 0.into());
            }
        }
        emit(
            &mut out,
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": self.stop_reason, "stop_sequence": null},
                "usage": usage,
            }),
        );
        emit(&mut out, json!({"type": "message_stop"}));
        self.stopped = true;
        out
    }

    fn start(&mut self, id: Option<&str>, out: &mut Vec<Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        let id = match id {
            Some(id) if !id.is_empty() => format!("msg_{id}"),
            _ => format!("msg_{}", unique_suffix()),
        };
        emit(
            out,
            json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }
            }),
        );
    }

    fn close(&mut self, out: &mut Vec<Bytes>) {
        if let Some(block) = self.open.take() {
            emit(out, json!({"type": "content_block_stop", "index": block.index()}));
        }
    }

    fn open_block(&mut self, block: impl FnOnce(u64) -> OpenBlock, content: Value, out: &mut Vec<Bytes>) -> u64 {
        self.close(out);
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(block(index));
        emit(out, json!({"type": "content_block_start", "index": index, "content_block": content}));
        index
    }

    fn text(&mut self, text: &str, out: &mut Vec<Bytes>) {
        let index = match self.open {
            Some(OpenBlock::Text { index }) => index,
            _ => self.open_block(|index| OpenBlock::Text { index }, json!({"type": "text", "text": ""}), out),
        };
        emit(
            out,
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": text},
            }),
        );
    }

    fn tool_call(&mut self, call: &Value, out: &mut Vec<Bytes>) {
        let call_index = call.get("index").and_then(Value::as_u64);
        let id = call.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
        let function = call.get("function");

        // Which call does this entry continue, if any? Some backends repeat
        // the id on every fragment; others reuse index 0 with fresh ids.
        let open_call = match self.open {
            Some(OpenBlock::Tool { call, .. }) => Some(call),
            _ => None,
        };
        let key = call_index.or(open_call);
        let known = key.and_then(|k| self.calls.iter().find(|c| c.call == k));
        let continues = match (known, id) {
            (Some(seen), Some(id)) => seen.id == id,
            (Some(_), None) => true,
            (None, _) => false,
        };

        let target = if continues {
            key
        } else {
            // No index at all: give it one that cannot collide.
            let call_key = call_index.unwrap_or(self.next_call);
            self.next_call = self.next_call.max(call_key.saturating_add(1));
            let id = id.map(str::to_string).unwrap_or_else(|| format!("toolu_{}", unique_suffix()));
            let name = function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or_default();
            self.calls.retain(|c| c.call != call_key);
            self.calls.push(SeenCall { call: call_key, id: id.clone() });
            self.open_block(
                |index| OpenBlock::Tool { index, call: call_key },
                json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
                out,
            );
            Some(call_key)
        };

        let Some(fragment) = function.and_then(|f| f.get("arguments")).and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            return;
        };
        if let Some(OpenBlock::Tool { index, call }) = self.open
            && Some(call) == target
        {
            emit(
                out,
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": fragment},
                }),
            );
        }
    }
}

fn emit(out: &mut Vec<Bytes>, event: Value) {
    let name = event.get("type").and_then(Value::as_str).unwrap_or("unknown").to_string();
    out.push(sse::named(&name, &event));
}

/// A process-unique suffix for ids the upstream did not supply.
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    format!("{nanos:x}{:04x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Session identity
// ---------------------------------------------------------------------------

/// Session id from `metadata.user_id` of the form
/// `user_<hash>_account_<uuid>_session_<uuid>` → the part after `_session_`;
/// None otherwise.
///
/// Newer Claude Code versions send `user_id` as a JSON-encoded object with a
/// `session_id` field; that form is accepted too.
pub fn session_from_metadata(req: &Value) -> Option<String> {
    let user_id = req.pointer("/metadata/user_id")?.as_str()?;
    if let Some(rest) = user_id.strip_prefix("user_") {
        let (_, after_account) = rest.split_once("_account_")?;
        let (_, session) = after_account.split_once("_session_")?;
        return valid_session(session);
    }
    let parsed: Value = serde_json::from_str(user_id).ok()?;
    valid_session(parsed.get("session_id")?.as_str()?)
}

fn valid_session(session: &str) -> Option<String> {
    let ok = !session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| session.to_string())
}
