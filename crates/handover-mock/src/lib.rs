//! handover-mock: a scripted, deterministic OpenAI-compatible worker.
//!
//! Used in-process by the proxy's integration tests (see [`spawn`]) and as a
//! binary behind a real router in the dev stack. Behaviour is scripted by
//! `#mock:` directive lines in the last user message (tool calls only while
//! the request ends with that user message, so tool loops terminate):
//!
//! - `#mock:text <text>`: assistant content (lines join with `\n`, default `OK.`)
//! - `#mock:reasoning <text>`: `reasoning_content` emitted before the content
//! - `#mock:tool <name> <json-args>`: one tool call per line
//! - `#mock:fault 429|503|slow:<ms>|rename:<model>|drop`: fault injection
//!
//! Extraction requests (`response_format.json_schema.name ==
//! "handover_extraction"`) answer with `{"deltas":[...]}` built from
//! `#mock:extract <json>` / `#mock:extract-raw <text>` lines in user messages
//! from index `HANDOVER-NEW-FROM: <k>` on, or from a tiny sentence heuristic.
//!
//! Every worker simulates a prefix cache and reports
//! `usage.prompt_tokens_details.cached_tokens`; requests are logged at
//! `/_mock/log`.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::stream::{self, BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Maximum characters per streamed content / reasoning / argument piece.
const PIECE_CHARS: usize = 16;
/// Maximum number of remembered prefix hashes per worker.
const CACHE_CAP: usize = 50_000;
/// `response_format.json_schema.name` that marks an extraction request.
const EXTRACTION_SCHEMA_NAME: &str = "handover_extraction";
/// Machine-readable marker in the extraction prompt.
const NEW_FROM_MARKER: &str = "HANDOVER-NEW-FROM:";
/// Delay before aborting a dropped body, so the partial output is flushed.
const DROP_DELAY: Duration = Duration::from_millis(20);

/// Configuration of one mock worker.
#[derive(Clone, Debug)]
pub struct MockConfig {
    /// Worker name recorded in every [`LogEntry`].
    pub worker: String,
    /// Served models. Empty means any model is accepted.
    pub models: Vec<String>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self { worker: "mock-0".to_owned(), models: Vec::new() }
    }
}

/// One handled `/v1/chat/completions` request, as returned by `GET /_mock/log`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    pub seq: u64,
    pub worker: String,
    pub session_id: Option<String>,
    pub model: String,
    pub served_model: String,
    /// `"chat"` or `"extraction"`.
    pub kind: String,
    pub stream: bool,
    pub messages: usize,
    pub prefix_hash: String,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub fault: Option<String>,
    /// Hex SHA-256 of the raw request body, to check byte-identical forwarding.
    #[serde(default)]
    pub body_sha256: String,
    /// Names of the tool definitions in the request.
    #[serde(default)]
    pub tools: Vec<String>,
    /// First line of the request's system prompt (which conversation it is).
    #[serde(default)]
    pub system_head: String,
    /// The system prompt carries a handover packet.
    #[serde(default)]
    pub packet: bool,
}

/// Build the mock's router. All requests to it share one in-memory state
/// (sequence counter, request log, prefix cache).
pub fn router(config: MockConfig) -> Router {
    // `HANDOVER_MOCK_DUMP_DIR`: save every request body there (for inspecting real harness traffic).
    let dump_dir = std::env::var_os("HANDOVER_MOCK_DUMP_DIR").map(std::path::PathBuf::from);
    let shared = Arc::new(Shared {
        config,
        seq: AtomicU64::new(0),
        log: Mutex::default(),
        cache: Mutex::default(),
        roots: Mutex::default(),
        dump_dir,
    });
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        .route("/_mock/log", get(get_log).delete(clear_log))
        .route("/_mock/root", post(set_root))
        .route("/health", get(health))
        // Like a real worker, accept whole long sessions (axum's default is 2 MB).
        .layer(axum::extract::DefaultBodyLimit::max(256 << 20))
        .with_state(shared)
}

/// Bind `127.0.0.1:0`, serve [`router`] on a background task and return the
/// bound address.
pub async fn spawn(config: MockConfig) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock listener");
    let addr = listener.local_addr().expect("mock listener address");
    let app = router(config);
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(%err, "handover-mock server failed");
        }
    });
    addr
}

// ---------------------------------------------------------------------------
// State

struct Shared {
    config: MockConfig,
    seq: AtomicU64,
    log: Mutex<Vec<LogEntry>>,
    cache: Mutex<PrefixCache>,
    /// `root` overrides for `/v1/models` (simulates swapping weights under one name).
    roots: Mutex<std::collections::HashMap<String, String>>,
    dump_dir: Option<std::path::PathBuf>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

type Hash = [u8; 32];

/// Set of message-prefix hashes seen by this worker, evicting the oldest.
#[derive(Default)]
struct PrefixCache {
    seen: HashSet<Hash>,
    order: VecDeque<Hash>,
}

impl PrefixCache {
    /// Index of the longest prefix already seen.
    fn longest_seen(&self, hashes: &[Hash]) -> Option<usize> {
        hashes.iter().rposition(|h| self.seen.contains(h))
    }

    fn record(&mut self, hashes: &[Hash]) {
        for hash in hashes {
            if self.seen.insert(*hash) {
                self.order.push_back(*hash);
                if self.order.len() > CACHE_CAP
                    && let Some(oldest) = self.order.pop_front()
                {
                    self.seen.remove(&oldest);
                }
            }
        }
    }
}

/// Per-prefix hashes and per-message token estimates of a request.
struct Prefix {
    /// `hashes[i]` = SHA-256 of `serde_json::to_string(&messages[0..=i])`.
    hashes: Vec<Hash>,
    /// `tokens[i]` = ceil(len(serde_json::to_string(&messages[i])) / 4).
    tokens: Vec<u64>,
}

impl Prefix {
    fn of(messages: &[Value]) -> Self {
        // The compact serialization of a slice is "[" + items joined by "," +
        // "]", so the prefix hashes can be computed incrementally.
        let mut hasher = Sha256::new();
        hasher.update(b"[");
        let mut hashes = Vec::with_capacity(messages.len());
        let mut tokens = Vec::with_capacity(messages.len());
        for (i, message) in messages.iter().enumerate() {
            let text = message.to_string();
            tokens.push(text.len().div_ceil(4) as u64);
            if i > 0 {
                hasher.update(b",");
            }
            hasher.update(text.as_bytes());
            let mut prefix = hasher.clone();
            prefix.update(b"]");
            hashes.push(prefix.finalize().into());
        }
        Self { hashes, tokens }
    }

    /// Hash of all messages but the last (or of the only message).
    fn log_hash(&self) -> String {
        let hash: Hash = match self.hashes.len() {
            0 => Sha256::digest(b"[]").into(),
            1 => self.hashes[0],
            n => self.hashes[n - 2],
        };
        hex::encode(hash)
    }

    fn total_tokens(&self) -> u64 {
        self.tokens.iter().sum()
    }

    fn tokens_through(&self, index: usize) -> u64 {
        self.tokens[..=index].iter().sum()
    }
}

// ---------------------------------------------------------------------------
// Small endpoints

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn list_models(State(shared): State<Arc<Shared>>) -> Json<Value> {
    let roots = lock(&shared.roots);
    let data: Vec<Value> = shared
        .config
        .models
        .iter()
        .map(|m| json!({"id": m, "object": "model", "root": roots.get(m).unwrap_or(m), "owned_by": "mock"}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}

/// `POST /_mock/root {"model": "...", "root": "..."}`: change what `/v1/models` reports as the model's root.
async fn set_root(State(shared): State<Arc<Shared>>, Json(body): Json<Value>) -> StatusCode {
    match (body.get("model").and_then(Value::as_str), body.get("root").and_then(Value::as_str)) {
        (Some(m), Some(r)) => {
            lock(&shared.roots).insert(m.to_owned(), r.to_owned());
            StatusCode::NO_CONTENT
        }
        _ => StatusCode::BAD_REQUEST,
    }
}

async fn get_log(State(shared): State<Arc<Shared>>) -> Json<Vec<LogEntry>> {
    Json(lock(&shared.log).clone())
}

async fn clear_log(State(shared): State<Arc<Shared>>) -> StatusCode {
    lock(&shared.log).clear();
    StatusCode::NO_CONTENT
}

fn error_response(status: StatusCode, message: &str, kind: &str) -> Response {
    (status, Json(json!({"error": {"message": message, "type": kind}}))).into_response()
}

fn fault_response(code: u16) -> Response {
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let (message, kind) = match code {
        429 => ("mock fault: too many requests", "RateLimitError"),
        503 => ("mock fault: service unavailable", "ServiceUnavailableError"),
        _ => ("mock fault", "MockFaultError"),
    };
    error_response(status, message, kind)
}

// ---------------------------------------------------------------------------
// Chat completions

async fn chat_completions(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("invalid JSON body: {err}"), "BadRequestError");
        }
    };
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return error_response(StatusCode::BAD_REQUEST, "`messages` must be an array", "BadRequestError");
    };
    let model = request.get("model").and_then(Value::as_str).unwrap_or_default().to_owned();
    if !shared.config.models.is_empty() && !shared.config.models.contains(&model) {
        return error_response(StatusCode::NOT_FOUND, "model not found", "NotFoundError");
    }
    let session_id = headers.get("x-session-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let stream = request.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let include_usage = request.pointer("/stream_options/include_usage").and_then(Value::as_bool).unwrap_or(false);
    let extraction = is_extraction(&request);

    // Directives come from the last user message; for extraction requests the
    // final message is the extraction prompt, so look before it.
    let script_source = if extraction { last_user(&messages[..messages.len().saturating_sub(1)]) } else { last_user(messages) };
    let mut script = script_source.map(Script::parse).unwrap_or_default();
    // After a tool result the scripted tool calls have been served: answer with
    // text instead, so a real harness's tool loop terminates.
    let last_role = messages.last().and_then(|m| m.get("role")).and_then(Value::as_str);
    if !extraction && last_role != Some("user") {
        script.tools.clear();
    }
    let faults = &script.faults;

    let seq = shared.seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(dir) = &shared.dump_dir {
        let kind = if extraction { "extraction" } else { "chat" };
        let record = json!({"seq": seq, "kind": kind, "x-session-id": session_id, "body": request});
        let path = dir.join(format!("{seq:04}-{kind}.json"));
        if let Err(err) = std::fs::write(&path, serde_json::to_vec_pretty(&record).unwrap_or_default()) {
            tracing::warn!(%err, path = %path.display(), "could not dump request");
        }
    }
    let prefix = Prefix::of(messages);
    let cached_tokens = {
        let mut cache = lock(&shared.cache);
        let cached = cache.longest_seen(&prefix.hashes).map_or(0, |i| prefix.tokens_through(i));
        // A rejected request never reaches prefill.
        if faults.status.is_none() {
            cache.record(&prefix.hashes);
        }
        cached
    };
    let prompt_tokens = prefix.total_tokens();
    let served_model = faults.rename.clone().unwrap_or_else(|| model.clone());

    let system_text: String = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("system"))
        .map(|m| match m.get("content") {
            Some(Value::String(t)) => t.clone(),
            Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let entry = LogEntry {
        seq,
        worker: shared.config.worker.clone(),
        session_id,
        model,
        served_model: served_model.clone(),
        kind: if extraction { "extraction" } else { "chat" }.to_owned(),
        stream,
        messages: messages.len(),
        prefix_hash: prefix.log_hash(),
        prompt_tokens,
        cached_tokens,
        fault: faults.label(),
        body_sha256: hex::encode(Sha256::digest(&body)),
        tools: request
            .get("tools")
            .and_then(Value::as_array)
            .map(|t| t.iter().filter_map(|x| x.pointer("/function/name").and_then(Value::as_str)).map(str::to_owned).collect())
            .unwrap_or_default(),
        system_head: system_text.lines().next().unwrap_or("").to_owned(),
        packet: system_text.contains("<handover-register"),
    };
    tracing::debug!(?entry, "handled chat completion");
    lock(&shared.log).push(entry);

    if let Some(code) = faults.status {
        if let Some(delay) = faults.slow {
            tokio::time::sleep(delay).await;
        }
        return fault_response(code);
    }

    let reply = if extraction { Reply::extraction(messages) } else { Reply::chat(&script, seq) };
    let usage = usage_json(prompt_tokens, cached_tokens, reply.completion_tokens());
    let ctx = Ctx { id: format!("chatcmpl-{seq}"), created: unix_now(), model: served_model };
    if stream {
        let events = sse_events(&ctx, &reply, include_usage.then_some(&usage));
        stream_response(events, faults)
    } else {
        full_response(&ctx, reply, usage, faults).await
    }
}

fn is_extraction(request: &Value) -> bool {
    request.pointer("/response_format/type").and_then(Value::as_str) == Some("json_schema")
        && request.pointer("/response_format/json_schema/name").and_then(Value::as_str) == Some(EXTRACTION_SCHEMA_NAME)
}

fn usage_json(prompt: u64, cached: u64, completion: u64) -> Value {
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
        "prompt_tokens_details": {"cached_tokens": cached},
    })
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------------------
// Messages and directives

fn is_user(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
}

fn last_user(messages: &[Value]) -> Option<&Value> {
    messages.iter().rev().find(|m| is_user(m))
}

/// Text of a message: a string `content`, or its `{"type":"text"}` parts.
fn message_texts(message: &Value) -> Vec<&str> {
    match message.get("content") {
        Some(Value::String(text)) => vec![text.as_str()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// `(name, rest)` of every `#mock:<name> <rest>` line in a message. `rest`
/// still starts with the separating whitespace, if any.
fn directives(message: &Value) -> Vec<(&str, &str)> {
    message_texts(message)
        .into_iter()
        .flat_map(str::lines)
        .filter_map(|line| {
            let body = line.trim_start().strip_prefix("#mock:")?;
            Some(match body.find(char::is_whitespace) {
                Some(i) => (&body[..i], &body[i..]),
                None => (body, ""),
            })
        })
        .collect()
}

/// Directive argument kept verbatim apart from the single separator.
fn verbatim(rest: &str) -> &str {
    rest.strip_prefix([' ', '\t']).unwrap_or(rest)
}

#[derive(Default)]
struct Script {
    text: Vec<String>,
    reasoning: Vec<String>,
    tools: Vec<(String, String)>,
    faults: Faults,
}

impl Script {
    fn parse(message: &Value) -> Self {
        let mut script = Self::default();
        for (name, rest) in directives(message) {
            match name {
                "text" => script.text.push(verbatim(rest).to_owned()),
                "reasoning" => script.reasoning.push(verbatim(rest).to_owned()),
                "tool" => {
                    let rest = rest.trim();
                    let (tool, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                    let args = args.trim();
                    let args = if args.is_empty() { "{}" } else { args };
                    script.tools.push((tool.to_owned(), args.to_owned()));
                }
                "fault" => script.faults.add(rest.trim()),
                _ => {}
            }
        }
        script
    }
}

#[derive(Default)]
struct Faults {
    status: Option<u16>,
    slow: Option<Duration>,
    rename: Option<String>,
    drop: bool,
    labels: Vec<String>,
}

impl Faults {
    fn add(&mut self, spec: &str) {
        if let Ok(code) = spec.parse::<u16>() {
            self.status = Some(code);
        } else if let Some(ms) = spec.strip_prefix("slow:") {
            let Ok(ms) = ms.trim().parse::<u64>() else {
                tracing::warn!(spec, "ignoring malformed mock fault");
                return;
            };
            self.slow = Some(Duration::from_millis(ms));
        } else if let Some(model) = spec.strip_prefix("rename:") {
            self.rename = Some(model.trim().to_owned());
        } else if spec == "drop" {
            self.drop = true;
        } else {
            tracing::warn!(spec, "ignoring unknown mock fault");
            return;
        }
        self.labels.push(spec.to_owned());
    }

    fn label(&self) -> Option<String> {
        (!self.labels.is_empty()).then(|| self.labels.join(","))
    }
}

// ---------------------------------------------------------------------------
// Replies

struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

struct Reply {
    reasoning: Option<String>,
    content: Option<String>,
    tool_calls: Vec<ToolCall>,
    finish_reason: &'static str,
}

impl Reply {
    fn chat(script: &Script, seq: u64) -> Self {
        let tool_calls: Vec<ToolCall> = script
            .tools
            .iter()
            .enumerate()
            .map(|(i, (name, arguments))| ToolCall { id: format!("call_{seq}_{i}"), name: name.clone(), arguments: arguments.clone() })
            .collect();
        let content = if !script.text.is_empty() {
            Some(script.text.join("\n"))
        } else if tool_calls.is_empty() {
            Some("OK.".to_owned())
        } else {
            None
        };
        let finish_reason = if tool_calls.is_empty() { "stop" } else { "tool_calls" };
        Self { reasoning: (!script.reasoning.is_empty()).then(|| script.reasoning.join("\n")), content, tool_calls, finish_reason }
    }

    fn extraction(messages: &[Value]) -> Self {
        Self { reasoning: None, content: Some(extraction_content(messages)), tool_calls: Vec::new(), finish_reason: "stop" }
    }

    fn completion_tokens(&self) -> u64 {
        let chars = self.reasoning.as_ref().map_or(0, String::len)
            + self.content.as_ref().map_or(0, String::len)
            + self.tool_calls.iter().map(|c| c.name.len() + c.arguments.len()).sum::<usize>();
        (chars as u64).div_ceil(4).max(1)
    }
}

// ---------------------------------------------------------------------------
// Extraction

fn extraction_content(messages: &[Value]) -> String {
    let Some((prompt, history)) = messages.split_last() else {
        return json!({"deltas": []}).to_string();
    };
    let from = new_from(prompt).unwrap_or(0);

    let mut raw: Vec<&str> = Vec::new();
    let mut deltas: Vec<Value> = Vec::new();
    let mut scripted = false;
    for message in history.iter().skip(from).filter(|m| is_user(m)) {
        for (name, rest) in directives(message) {
            match name {
                "extract" => {
                    scripted = true;
                    match serde_json::from_str::<Value>(rest.trim()) {
                        Ok(delta) => deltas.push(delta),
                        Err(err) => tracing::warn!(%err, "ignoring malformed #mock:extract"),
                    }
                }
                "extract-raw" => raw.push(verbatim(rest)),
                _ => {}
            }
        }
    }
    if !raw.is_empty() {
        return raw.join("\n");
    }
    if !scripted {
        deltas = heuristic_deltas(history, from);
    }
    json!({"deltas": deltas}).to_string()
}

/// `k` from the `HANDOVER-NEW-FROM: <k>` line of the extraction prompt.
fn new_from(prompt: &Value) -> Option<usize> {
    message_texts(prompt).into_iter().find_map(|text| {
        let at = text.find(NEW_FROM_MARKER)?;
        let rest = text[at + NEW_FROM_MARKER.len()..].trim_start();
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        rest[..end].parse().ok()
    })
}

fn heuristic_deltas(history: &[Value], from: usize) -> Vec<Value> {
    let mut deltas = Vec::new();
    for (index, message) in history.iter().enumerate().skip(from) {
        if !is_user(message) {
            continue;
        }
        for text in message_texts(message) {
            let cleaned = strip_system_reminders(text);
            for line in cleaned.lines() {
                if line.trim_start().starts_with("#mock:") {
                    continue;
                }
                for sentence in line.split(['.', '!', '?']) {
                    if let Some(delta) = heuristic_delta(sentence.trim(), index) {
                        deltas.push(delta);
                    }
                }
            }
        }
    }
    deltas
}

/// Replace every `<system-reminder>...</system-reminder>` span by a newline.
fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        out.push('\n');
        rest = match rest[start..].find(CLOSE) {
            Some(end) => &rest[start + end + CLOSE.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn heuristic_delta(sentence: &str, index: usize) -> Option<Value> {
    const PROHIBITIONS: [&str; 4] = ["do not ", "don't ", "don\u{2019}t ", "never "];
    const PERMISSIONS: [&str; 3] = ["you may ", "you can ", "feel free to "];

    let (kind, strength, statement) = if let Some(prefix) = PROHIBITIONS.iter().find(|p| starts_with_ignore_case(sentence, p)) {
        let rest = &sentence[prefix.len()..];
        ("prohibition", "hard", format!("Do not {rest}"))
    } else if PERMISSIONS.iter().any(|p| starts_with_ignore_case(sentence, p)) {
        ("permission", "graded", capitalize(sentence))
    } else {
        return None;
    };

    let mut delta = json!({
        "op": "add",
        "item": "new",
        "kind": kind,
        "strength": strength,
        "statement": statement,
        "source_message": index,
        "quote": sentence,
        "confidence": 0.9,
    });
    let paths: Vec<String> = sentence
        .split_whitespace()
        .map(|token| token.trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | '(' | ')' | ',' | ';' | ':' | '*')))
        .filter(|token| token.len() > 1 && token.ends_with('/'))
        .map(|token| format!("{token}**"))
        .collect();
    if !paths.is_empty() {
        delta["enforce"] = json!({"ops": ["write", "exec"], "paths": paths});
    }
    Some(delta)
}

// ---------------------------------------------------------------------------
// Response encoding

struct Ctx {
    id: String,
    created: u64,
    model: String,
}

impl Ctx {
    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
    }
}

/// Split into pieces of at most [`PIECE_CHARS`] characters.
fn pieces(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(PIECE_CHARS).map(|piece| piece.iter().collect()).collect()
}

fn sse_events(ctx: &Ctx, reply: &Reply, usage: Option<&Value>) -> Vec<Bytes> {
    let mut chunks = vec![ctx.chunk(json!({"role": "assistant", "content": ""}), None)];
    if let Some(reasoning) = &reply.reasoning {
        for piece in pieces(reasoning) {
            chunks.push(ctx.chunk(json!({"reasoning_content": piece}), None));
        }
    }
    if let Some(content) = &reply.content {
        for piece in pieces(content) {
            chunks.push(ctx.chunk(json!({"content": piece}), None));
        }
    }
    for (i, call) in reply.tool_calls.iter().enumerate() {
        chunks.push(ctx.chunk(
            json!({"tool_calls": [{
                "index": i,
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": ""},
            }]}),
            None,
        ));
        for piece in pieces(&call.arguments) {
            chunks.push(ctx.chunk(json!({"tool_calls": [{"index": i, "function": {"arguments": piece}}]}), None));
        }
    }
    chunks.push(ctx.chunk(json!({}), Some(reply.finish_reason)));
    if let Some(usage) = usage {
        chunks.push(json!({
            "id": ctx.id,
            "object": "chat.completion.chunk",
            "created": ctx.created,
            "model": ctx.model,
            "choices": [],
            "usage": usage,
        }));
    }
    let mut events: Vec<Bytes> = chunks.iter().map(|chunk| Bytes::from(format!("data: {chunk}\n\n"))).collect();
    events.push(Bytes::from_static(b"data: [DONE]\n\n"));
    events
}

type BodyStream = BoxStream<'static, Result<Bytes, std::io::Error>>;

/// A body that sends `first`, then aborts the connection.
fn dropped_body(first: Bytes, delay: Option<Duration>) -> BodyStream {
    stream::once(async move {
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        Ok(first)
    })
    .chain(stream::once(async {
        tokio::time::sleep(DROP_DELAY).await;
        Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "mock fault: drop"))
    }))
    .boxed()
}

fn stream_response(events: Vec<Bytes>, faults: &Faults) -> Response {
    let slow = faults.slow;
    let body: BodyStream = if faults.drop {
        let first = events.into_iter().next().unwrap_or_default();
        dropped_body(first, slow)
    } else {
        stream::iter(events)
            .then(move |event| async move {
                if let Some(delay) = slow {
                    tokio::time::sleep(delay).await;
                }
                Ok(event)
            })
            .boxed()
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body))
        .expect("valid SSE response")
}

async fn full_response(ctx: &Ctx, reply: Reply, usage: Value, faults: &Faults) -> Response {
    if let Some(delay) = faults.slow {
        tokio::time::sleep(delay).await;
    }
    let mut message = json!({"role": "assistant", "content": reply.content});
    if !reply.tool_calls.is_empty() {
        let calls: Vec<Value> = reply
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments},
                })
            })
            .collect();
        message["tool_calls"] = Value::Array(calls);
    }
    if let Some(reasoning) = reply.reasoning {
        message["reasoning_content"] = Value::String(reasoning);
    }
    let body = json!({
        "id": ctx.id,
        "object": "chat.completion",
        "created": ctx.created,
        "model": ctx.model,
        "choices": [{
            "index": 0,
            "message": message,
            "logprobs": null,
            "finish_reason": reply.finish_reason,
        }],
        "usage": usage,
    });
    if faults.drop {
        let text = body.to_string();
        let partial = Bytes::copy_from_slice(&text.as_bytes()[..text.len() / 2]);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(dropped_body(partial, None)))
            .expect("valid JSON response");
    }
    Json(body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_prefix_hash_matches_slice_serialization() {
        let messages = vec![
            json!({"role": "system", "content": "be terse"}),
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
            json!({"role": "assistant", "content": null, "tool_calls": []}),
        ];
        let prefix = Prefix::of(&messages);
        for i in 0..messages.len() {
            let direct: Hash = Sha256::digest(serde_json::to_string(&messages[..=i]).unwrap()).into();
            assert_eq!(prefix.hashes[i], direct);
        }
        assert_eq!(prefix.log_hash(), hex::encode(prefix.hashes[1]));
    }

    #[test]
    fn heuristic_normalizes_and_finds_paths() {
        let delta = heuristic_delta("don't touch `migrations/` please", 4).unwrap();
        assert_eq!(delta["statement"], "Do not touch `migrations/` please");
        assert_eq!(delta["quote"], "don't touch `migrations/` please");
        assert_eq!(delta["enforce"]["paths"], json!(["migrations/**"]));
        let delta = heuristic_delta("feel free to refactor", 2).unwrap();
        assert_eq!(delta["kind"], "permission");
        assert_eq!(delta["statement"], "Feel free to refactor");
        assert!(heuristic_delta("please do not worry", 1).is_none());
    }

    #[test]
    fn system_reminders_are_stripped() {
        assert_eq!(strip_system_reminders("a<system-reminder>x</system-reminder>b<system-reminder>y"), "a\nb\n");
    }
}
