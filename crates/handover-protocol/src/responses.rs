//! OpenAI Responses adapter (PLAN.md §13).
//!
//! Codex CLI speaks the Responses API with `store: false`: every turn resends
//! the full input, so nothing has to be remembered between requests. When the
//! next hop only understands Chat Completions, the proxy translates the request
//! with [`request_to_chat`] and the answer back with [`completion_to_response`]
//! (non-streaming) or a [`StreamTranslator`] (streaming). Stateful requests
//! (`previous_response_id`) are rejected.
//!
//! The built-in `local_shell` tool has no Chat Completions equivalent. It is
//! offered to the next hop as a function tool of the same name whose arguments
//! are `{"action": <local shell action>}`, and calls to that function come back
//! as `local_shell_call` items.
//!
//! Every function here is total: malformed input yields an `Err` (only where a
//! request cannot be translated at all) or is skipped, never a panic.

use bytes::Bytes;
use serde_json::{Map, Value, json};

use crate::sse;

/// Name of the function tool that stands in for the built-in `local_shell`.
const LOCAL_SHELL: &str = "local_shell";

const LOCAL_SHELL_DESCRIPTION: &str = "Run a command on the user's machine. `action` is \
    {\"type\": \"exec\", \"command\": [program, args...], \"working_directory\": optional path, \
    \"timeout_ms\": optional number, \"env\": optional object of strings}.";

// ---------------------------------------------------------------------------
// Request: Responses -> Chat Completions
// ---------------------------------------------------------------------------

/// Responses request body → OpenAI Chat Completions request body.
///
/// Fails when `previous_response_id` is set (stateful; unsupported in v1) or
/// `input` is missing or neither a string nor an array. Unknown items and
/// parts, `reasoning` items and built-in tools other than `local_shell` are
/// skipped.
pub fn request_to_chat(req: &Value) -> Result<Value, String> {
    if req.get("previous_response_id").is_some_and(|id| !id.is_null() && id.as_str() != Some("")) {
        return Err("`previous_response_id` is not supported; resend the full input with `store: false`".into());
    }
    let input = req.get("input").ok_or("request has no `input`")?;

    let mut messages = Vec::new();
    if let Some(instructions) = req.get("instructions").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    match input {
        Value::String(text) => messages.push(json!({"role": "user", "content": text})),
        Value::Array(items) => input_items(items, &mut messages),
        _ => return Err("`input` is neither a string nor an array".into()),
    }

    let mut out = Map::new();
    for key in ["model", "temperature", "top_p"] {
        if let Some(v) = req.get(key).filter(|v| !v.is_null()) {
            out.insert(key.to_string(), v.clone());
        }
    }
    if let Some(v) = req.get("max_output_tokens").filter(|v| !v.is_null()) {
        out.insert("max_tokens".into(), v.clone());
    }
    merge_leading_system(&mut messages);
    out.insert("messages".into(), Value::Array(messages));

    let tools: Vec<Value> =
        req.get("tools").and_then(Value::as_array).map(|tools| tools.iter().filter_map(tool).collect()).unwrap_or_default();
    // Chat Completions rejects `tool_choice` and `parallel_tool_calls` without tools.
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = req.get("tool_choice").and_then(tool_choice) {
            out.insert("tool_choice".into(), choice);
        }
        if let Some(parallel) = req.get("parallel_tool_calls").filter(|v| v.is_boolean()) {
            out.insert("parallel_tool_calls".into(), parallel.clone());
        }
    }

    if req.get("stream").and_then(Value::as_bool) == Some(true) {
        out.insert("stream".into(), Value::Bool(true));
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Ok(Value::Object(out))
}

/// An assistant message assembled from a run of input items: an optional
/// assistant `message` followed by any number of tool calls.
#[derive(Default)]
struct Assistant {
    text: String,
    tool_calls: Vec<Value>,
}

/// Push the assembled assistant message, if any, unless it is empty.
fn flush(assistant: &mut Option<Assistant>, out: &mut Vec<Value>) {
    let Some(Assistant { text, tool_calls }) = assistant.take() else { return };
    if text.is_empty() && tool_calls.is_empty() {
        return;
    }
    let mut message = Map::new();
    message.insert("role".into(), "assistant".into());
    message.insert("content".into(), if text.is_empty() { Value::Null } else { Value::String(text) });
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    out.push(Value::Object(message));
}

fn input_items(items: &[Value], out: &mut Vec<Value>) {
    let mut assistant: Option<Assistant> = None;
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") | None => {
                let content = item.get("content");
                match item.get("role").and_then(Value::as_str) {
                    Some("assistant") => {
                        flush(&mut assistant, out);
                        assistant = Some(Assistant { text: joined_text(&content_parts(content)), tool_calls: Vec::new() });
                    }
                    Some(role @ ("user" | "system" | "developer")) => {
                        flush(&mut assistant, out);
                        out.extend(plain_message(role, content));
                    }
                    _ => {}
                }
            }
            Some("function_call") => {
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                let call = tool_call(call_id(item), name, arguments_json(item.get("arguments")));
                assistant.get_or_insert_with(Assistant::default).tool_calls.push(call);
            }
            Some("local_shell_call") => {
                let action = item.get("action").filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
                let arguments = serde_json::to_string(&json!({"action": action})).unwrap_or_else(|_| "{}".into());
                let call = tool_call(call_id(item), LOCAL_SHELL, arguments);
                assistant.get_or_insert_with(Assistant::default).tool_calls.push(call);
            }
            Some("function_call_output" | "local_shell_call_output") => {
                flush(&mut assistant, out);
                out.push(json!({"role": "tool", "tool_call_id": call_id(item), "content": output_text(item.get("output"))}));
            }
            _ => {} // reasoning, unknown items
        }
    }
    flush(&mut assistant, out);
}

/// Chat content parts (`text` and `image_url`) of a Responses message's
/// `content`, in order. Empty texts and unknown parts are skipped.
fn content_parts(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) if !s.is_empty() => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str)? {
                "input_text" | "output_text" | "text" => {
                    part.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()).map(|t| json!({"type": "text", "text": t}))
                }
                "input_image" => image_part(part),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn is_text_part(part: &Value) -> bool {
    part.get("type").and_then(Value::as_str) == Some("text")
}

/// Texts of the `text` parts, joined with a blank line.
fn joined_text(parts: &[Value]) -> String {
    parts.iter().filter(|p| is_text_part(p)).filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n\n")
}

/// `input_image` (with `image_url` as a string or `{url}`) → Chat image part.
fn image_part(part: &Value) -> Option<Value> {
    let url = match part.get("image_url")? {
        Value::String(url) => url.as_str(),
        other => other.get("url")?.as_str()?,
    };
    if url.is_empty() {
        return None;
    }
    let mut image_url = Map::new();
    image_url.insert("url".into(), url.into());
    if let Some(detail) = part.get("detail").and_then(Value::as_str) {
        image_url.insert("detail".into(), detail.into());
    }
    Some(json!({"type": "image_url", "image_url": image_url}))
}

/// A user, system or developer message. Images are kept for user messages
/// only (the content becomes a part array); `developer` becomes `system`.
fn plain_message(role: &str, content: Option<&Value>) -> Option<Value> {
    let parts = content_parts(content);
    // Several parts stay separate for user messages: injected blocks
    // (`<user_instructions>`, `<environment_context>`) are labeled per part.
    if role == "user" && (parts.iter().any(|p| !is_text_part(p)) || parts.len() > 1) {
        return Some(json!({"role": "user", "content": parts}));
    }
    let text = joined_text(&parts);
    if text.is_empty() {
        return None;
    }
    let role = if role == "user" { "user" } else { "system" };
    Some(json!({"role": role, "content": text}))
}

/// A tool call's id: `call_id`, falling back to `id` (which the Responses API
/// uses for `local_shell_call_output`).
fn call_id(item: &Value) -> &str {
    ["call_id", "id"].iter().find_map(|key| item.get(*key).and_then(Value::as_str).filter(|s| !s.is_empty())).unwrap_or_default()
}

/// Tool-call arguments as a JSON string: non-empty strings pass through, other
/// JSON objects or arrays are serialized, anything else becomes `{}`.
fn arguments_json(arguments: Option<&Value>) -> String {
    match arguments {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(v @ (Value::Object(_) | Value::Array(_))) => v.to_string(),
        _ => "{}".into(),
    }
}

fn tool_call(id: &str, name: &str, arguments: String) -> Value {
    json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments}})
}

/// A tool's output as text: strings pass through, part arrays contribute
/// their text, other JSON is serialized.
fn output_text(output: Option<&Value>) -> String {
    match output {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(_)) => joined_text(&content_parts(output)),
        Some(other) => other.to_string(),
    }
}

fn tool(tool: &Value) -> Option<Value> {
    match tool.get("type").and_then(Value::as_str)? {
        "function" => {
            let name = tool.get("name").and_then(Value::as_str).filter(|n| !n.is_empty())?;
            let mut function = Map::new();
            function.insert("name".into(), name.into());
            if let Some(description) = tool.get("description").and_then(Value::as_str) {
                function.insert("description".into(), description.into());
            }
            let parameters =
                tool.get("parameters").filter(|p| p.is_object()).cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            function.insert("parameters".into(), parameters);
            Some(json!({"type": "function", "function": function}))
        }
        "local_shell" => Some(json!({"type": "function", "function": {
            "name": LOCAL_SHELL,
            "description": LOCAL_SHELL_DESCRIPTION,
            "parameters": {"type": "object", "properties": {"action": {"type": "object"}}},
        }})),
        _ => None, // web_search, file_search, custom, ...: no Chat equivalent
    }
}

fn tool_choice(choice: &Value) -> Option<Value> {
    if let Some(mode) = choice.as_str() {
        return matches!(mode, "auto" | "none" | "required").then(|| choice.clone());
    }
    let name = match choice.get("type").and_then(Value::as_str)? {
        "function" => choice.get("name").and_then(Value::as_str).filter(|n| !n.is_empty())?,
        "local_shell" => LOCAL_SHELL,
        _ => return None,
    };
    Some(json!({"type": "function", "function": {"name": name}}))
}

// ---------------------------------------------------------------------------
// Response: Chat Completions -> Responses
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    InProgress,
    Completed,
    /// With the `incomplete_details.reason`.
    Incomplete(&'static str),
}

impl Status {
    fn from_finish(finish: Option<&str>) -> Self {
        match finish {
            Some("length") => Status::Incomplete("max_output_tokens"),
            Some("content_filter") => Status::Incomplete("content_filter"),
            _ => Status::Completed,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Status::InProgress => "in_progress",
            Status::Completed => "completed",
            Status::Incomplete(_) => "incomplete",
        }
    }
}

fn response_object(id: &str, created_at: u64, model: Value, status: Status, output: Vec<Value>, usage: Value) -> Value {
    let mut response = Map::new();
    response.insert("id".into(), id.into());
    response.insert("object".into(), "response".into());
    response.insert("created_at".into(), created_at.into());
    response.insert("status".into(), status.name().into());
    if let Status::Incomplete(reason) = status {
        response.insert("incomplete_details".into(), json!({"reason": reason}));
    }
    response.insert("model".into(), model);
    response.insert("output".into(), Value::Array(output));
    response.insert("usage".into(), usage);
    Value::Object(response)
}

fn u64_at(v: &Value, pointer: &str) -> Option<u64> {
    v.pointer(pointer).and_then(Value::as_u64)
}

/// Chat Completions usage → Responses usage; null when there is none. Both
/// count cached tokens inside the input total.
fn usage_of(usage: Option<&Value>) -> Value {
    let Some(usage) = usage.filter(|u| u.is_object()) else { return Value::Null };
    let input = u64_at(usage, "/prompt_tokens").unwrap_or(0);
    let output = u64_at(usage, "/completion_tokens").unwrap_or(0);
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": u64_at(usage, "/prompt_tokens_details/cached_tokens").unwrap_or(0)},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": u64_at(usage, "/completion_tokens_details/reasoning_tokens").unwrap_or(0)},
        "total_tokens": u64_at(usage, "/total_tokens").unwrap_or(input.saturating_add(output)),
    })
}

fn output_text_part(text: &str) -> Value {
    json!({"type": "output_text", "text": text, "annotations": []})
}

fn message_item(id: &str, status: Status, content: Vec<Value>) -> Value {
    json!({"type": "message", "id": id, "status": status.name(), "role": "assistant", "content": content})
}

fn call_item_id(call_id: &str, name: &str) -> String {
    if name == LOCAL_SHELL { format!("lsh_{call_id}") } else { format!("fc_{call_id}") }
}

/// A `function_call` item, or a `local_shell_call` item for a call to the
/// `local_shell` stand-in (its `action` taken from the arguments).
fn call_item(call_id: &str, name: &str, arguments: &str, status: Status) -> Value {
    let id = call_item_id(call_id, name);
    if name == LOCAL_SHELL {
        json!({"type": "local_shell_call", "id": id, "call_id": call_id, "action": shell_action(arguments), "status": status.name()})
    } else {
        json!({"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": arguments, "status": status.name()})
    }
}

/// The `action` object of `local_shell` arguments; `{}` when they do not parse
/// or have none.
fn shell_action(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|args| args.get("action").filter(|a| a.is_object()).cloned())
        .unwrap_or_else(|| json!({}))
}

/// Non-streaming Chat Completions response → Responses `response` object.
pub fn completion_to_response(chat: &Value) -> Value {
    let id = chat.get("id").and_then(Value::as_str).unwrap_or_default();
    let choice = chat.pointer("/choices/0");
    let message = choice.and_then(|c| c.get("message"));

    let mut output = Vec::new();
    if let Some(text) = message.and_then(|m| m.get("content")).and_then(Value::as_str).filter(|t| !t.is_empty()) {
        output.push(message_item(&format!("msg_{id}"), Status::Completed, vec![output_text_part(text)]));
    }
    let tool_calls = message.and_then(|m| m.get("tool_calls")).and_then(Value::as_array);
    for call in tool_calls.into_iter().flatten() {
        let function = call.get("function");
        let call_id = call.get("id").and_then(Value::as_str).unwrap_or_default();
        let name = function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or_default();
        let arguments = arguments_json(function.and_then(|f| f.get("arguments")));
        output.push(call_item(call_id, name, &arguments, Status::Completed));
    }

    let finish = choice.and_then(|c| c.get("finish_reason")).and_then(Value::as_str);
    response_object(
        &format!("resp_{id}"),
        chat.get("created").and_then(Value::as_u64).unwrap_or(0),
        chat.get("model").cloned().unwrap_or(Value::Null),
        Status::from_finish(finish),
        output,
        usage_of(chat.get("usage")),
    )
}

// ---------------------------------------------------------------------------
// Streaming: Chat Completions chunks -> Responses SSE events
// ---------------------------------------------------------------------------

/// The output item currently receiving deltas.
#[derive(Debug)]
enum OpenItem {
    Text { output_index: u64, id: String, text: String },
    Call { output_index: u64, key: u64, call_id: String, name: String, arguments: String },
}

/// A tool call seen in the stream, keyed by its Chat Completions `index`.
#[derive(Debug)]
struct SeenCall {
    key: u64,
    id: String,
}

/// Streaming translation of Chat Completions chunks into Responses SSE events.
///
/// Feed every parsed `chat.completion.chunk` to [`chunk`](Self::chunk) and call
/// [`finish`](Self::finish) once at the end of the stream. Every event carries
/// a `sequence_number` counting from 0. One output item is open at a time;
/// argument fragments for a tool call whose item was already closed
/// (interleaved parallel calls) are dropped, since its `output_item.done` has
/// been sent.
#[derive(Debug)]
pub struct StreamTranslator {
    model: String,
    started: bool,
    completed: bool,
    sequence: u64,
    /// Upstream completion id (or a generated one); response and message ids derive from it.
    id: String,
    created_at: u64,
    /// Closed output items, in order.
    output: Vec<Value>,
    open: Option<OpenItem>,
    calls: Vec<SeenCall>,
    next_call: u64,
    status: Status,
    usage: Value,
}

impl StreamTranslator {
    /// `model` is the model name reported in the response objects.
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            started: false,
            completed: false,
            sequence: 0,
            id: String::new(),
            created_at: 0,
            output: Vec::new(),
            open: None,
            calls: Vec::new(),
            next_call: 0,
            status: Status::Completed,
            usage: Value::Null,
        }
    }

    /// One parsed `chat.completion.chunk` JSON value → zero or more Responses
    /// SSE events.
    pub fn chunk(&mut self, chunk: &Value) -> Vec<Bytes> {
        let mut out = Vec::new();
        if self.completed {
            return out;
        }
        self.start(Some(chunk), &mut out);

        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = usage_of(Some(usage));
        }

        let choices = chunk.get("choices").and_then(Value::as_array);
        let choice = choices.and_then(|cs| cs.iter().find(|c| c.get("index").and_then(Value::as_u64) == Some(0)).or_else(|| cs.first()));
        let Some(choice) = choice else { return out };

        if let Some(delta) = choice.get("delta") {
            // `reasoning_content` is dropped.
            if let Some(text) = delta.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                self.text(text, &mut out);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.tool_call(call, &mut out);
                }
            }
        }

        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            self.status = Status::from_finish(Some(finish));
            self.close(&mut out);
        }
        out
    }

    /// Call once at end of stream (after `[DONE]` or EOF): closes any open item
    /// and emits `response.completed` if not yet emitted.
    pub fn finish(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        if self.completed {
            return out;
        }
        self.start(None, &mut out);
        self.close(&mut out);
        let response = self.response(self.status);
        self.emit(&mut out, json!({"type": "response.completed", "response": response}));
        self.completed = true;
        out
    }

    /// The response object with the items closed so far.
    fn response(&self, status: Status) -> Value {
        response_object(
            &format!("resp_{}", self.id),
            self.created_at,
            Value::String(self.model.clone()),
            status,
            self.output.clone(),
            self.usage.clone(),
        )
    }

    fn emit(&mut self, out: &mut Vec<Bytes>, mut event: Value) {
        let name = event.get("type").and_then(Value::as_str).unwrap_or("unknown").to_string();
        if let Value::Object(fields) = &mut event {
            fields.insert("sequence_number".into(), self.sequence.into());
        }
        self.sequence += 1;
        out.push(sse::named(&name, &event));
    }

    fn start(&mut self, chunk: Option<&Value>, out: &mut Vec<Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        let field = |key: &str| chunk.and_then(|c| c.get(key));
        self.id = field("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map_or_else(unique_suffix, str::to_string);
        self.created_at = field("created").and_then(Value::as_u64).unwrap_or(0);
        let response = self.response(Status::InProgress);
        self.emit(out, json!({"type": "response.created", "response": response.clone()}));
        self.emit(out, json!({"type": "response.in_progress", "response": response}));
    }

    fn close(&mut self, out: &mut Vec<Bytes>) {
        match self.open.take() {
            None => {}
            Some(OpenItem::Text { output_index, id, text }) => {
                self.emit(
                    out,
                    json!({
                        "type": "response.output_text.done",
                        "item_id": id, "output_index": output_index, "content_index": 0, "text": text,
                    }),
                );
                self.emit(
                    out,
                    json!({
                        "type": "response.content_part.done",
                        "item_id": id, "output_index": output_index, "content_index": 0, "part": output_text_part(&text),
                    }),
                );
                let item = message_item(&id, Status::Completed, vec![output_text_part(&text)]);
                self.emit(out, json!({"type": "response.output_item.done", "output_index": output_index, "item": item.clone()}));
                self.output.push(item);
            }
            Some(OpenItem::Call { output_index, call_id, name, arguments, .. }) => {
                let arguments = arguments_json(Some(&Value::String(arguments)));
                self.emit(
                    out,
                    json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": call_item_id(&call_id, &name), "output_index": output_index, "arguments": arguments,
                    }),
                );
                let item = call_item(&call_id, &name, &arguments, Status::Completed);
                self.emit(out, json!({"type": "response.output_item.done", "output_index": output_index, "item": item.clone()}));
                self.output.push(item);
            }
        }
    }

    fn next_output_index(&self) -> u64 {
        self.output.len() as u64
    }

    fn text(&mut self, text: &str, out: &mut Vec<Bytes>) {
        if !matches!(self.open, Some(OpenItem::Text { .. })) {
            self.close(out);
            let output_index = self.next_output_index();
            // The first item matches the non-streaming message id.
            let id = if output_index == 0 { format!("msg_{}", self.id) } else { format!("msg_{}_{output_index}", self.id) };
            self.emit(
                out,
                json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": message_item(&id, Status::InProgress, Vec::new()),
                }),
            );
            self.emit(
                out,
                json!({
                    "type": "response.content_part.added",
                    "item_id": id, "output_index": output_index, "content_index": 0, "part": output_text_part(""),
                }),
            );
            self.open = Some(OpenItem::Text { output_index, id, text: String::new() });
        }
        let event = match &mut self.open {
            Some(OpenItem::Text { output_index, id, text: full }) => {
                full.push_str(text);
                json!({
                    "type": "response.output_text.delta",
                    "item_id": id, "output_index": *output_index, "content_index": 0, "delta": text,
                })
            }
            _ => return,
        };
        self.emit(out, event);
    }

    fn tool_call(&mut self, call: &Value, out: &mut Vec<Bytes>) {
        let call_index = call.get("index").and_then(Value::as_u64);
        let id = call.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
        let function = call.get("function");

        // Which call does this entry continue, if any? Some backends repeat
        // the id on every fragment; others reuse index 0 with fresh ids.
        let open_call = match &self.open {
            Some(OpenItem::Call { key, .. }) => Some(*key),
            _ => None,
        };
        let key = call_index.or(open_call);
        let known = key.and_then(|k| self.calls.iter().find(|c| c.key == k));
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
            let call_id = id.map_or_else(|| format!("call_{}", unique_suffix()), str::to_string);
            let name = function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or_default().to_string();
            self.calls.retain(|c| c.key != call_key);
            self.calls.push(SeenCall { key: call_key, id: call_id.clone() });

            self.close(out);
            let output_index = self.next_output_index();
            self.emit(
                out,
                json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": call_item(&call_id, &name, "", Status::InProgress),
                }),
            );
            self.open = Some(OpenItem::Call { output_index, key: call_key, call_id, name, arguments: String::new() });
            Some(call_key)
        };

        let Some(fragment) = function.and_then(|f| f.get("arguments")).and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            return;
        };
        let event = match &mut self.open {
            Some(OpenItem::Call { output_index, key, call_id, name, arguments }) if Some(*key) == target => {
                arguments.push_str(fragment);
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": call_item_id(call_id, name), "output_index": *output_index, "delta": fragment,
                })
            }
            _ => return,
        };
        self.emit(out, event);
    }
}

/// A process-unique suffix for ids the upstream did not supply.
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    format!("{nanos:x}{:04x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// `instructions` plus Codex's developer message would be two system messages;
/// strict chat templates accept only one, at the start. Merge the leading run
/// of plain-text system messages into one.
fn merge_leading_system(messages: &mut Vec<Value>) {
    let run = messages
        .iter()
        .take_while(|m| m.get("role").and_then(Value::as_str) == Some("system") && m.get("content").is_some_and(Value::is_string))
        .count();
    if run < 2 {
        return;
    }
    let text: Vec<String> = messages.drain(..run).filter_map(|m| m.get("content").and_then(Value::as_str).map(str::to_string)).collect();
    messages.insert(0, json!({"role": "system", "content": text.join("\n\n")}));
}
