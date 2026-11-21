//! The handover proxy (PLAN.md §4–§13).
//!
//! Sits in front of any OpenAI-compatible next hop. A session that never crosses
//! a boundary is forwarded byte-identically; at a handover the proxy injects a
//! status-aware packet, enforces hard constraints on tool calls, keeps the
//! register current by extraction, and audits its own duties in Positorium.

pub mod admin;
pub mod config;
pub mod extract;
pub mod metrics;
pub mod pipeline;
pub mod session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use handover_core::Conversation;
use handover_core::clock::Clock;
use handover_core::profile::Profile;
use handover_protocol::sse::SseParser;
use handover_protocol::{messages, responses};
use handover_register::Register;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;

pub use config::Config;
use pipeline::{AppState, Ingress, Shared, Upstreamed};

pub fn err(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"error": {"message": msg, "type": "handover_error"}}))).into_response()
}

/// Bearer token from `Authorization` or Anthropic's `x-api-key`.
pub fn bearer(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        return Some(v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")).unwrap_or(v).trim().to_string());
    }
    headers.get("x-api-key").and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string())
}

/// Build the shared state: open the store, load profiles.
pub fn build(config: Config) -> anyhow::Result<Shared> {
    let register = Register::open(config.store.path.as_deref())?;
    let mut profiles = HashMap::new();
    for name in Profile::builtin_names() {
        profiles.insert(name.to_string(), Profile::builtin(name).expect("builtin"));
    }
    for (name, path) in &config.profiles {
        let p = Profile::from_toml(&std::fs::read_to_string(path)?)?;
        profiles.insert(name.clone(), p);
    }
    let developers = config.developers.iter().map(|d| (d.token_sha256.to_lowercase(), d.label.clone())).collect();
    let admin_tokens =
        config.developers.iter().filter_map(|d| d.admin_token_sha256.as_ref().map(|h| (h.to_lowercase(), d.label.clone()))).collect();
    let permits = config.extractor.max_concurrent.max(1);
    Ok(Arc::new(AppState {
        register,
        http: reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(10)).build()?,
        sessions: Mutex::new(HashMap::new()),
        clock: Clock::new(),
        metrics: Default::default(),
        extract_permits: Arc::new(Semaphore::new(permits)),
        profiles,
        developers,
        admin_tokens,
        overloaded_until: Mutex::new(None),
        model_roots: Mutex::new(HashMap::new()),
        config,
    }))
}

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/responses", post(openai_responses))
        .route("/v1/models", get(models))
        .route("/v1/sessions", get(admin::list))
        .route("/v1/sessions/{key}", get(admin::show))
        .route("/v1/sessions/{key}/items", post(admin::decide))
        .route("/v1/sessions/{key}/audit", post(admin::audit))
        .route("/v1/sessions/{key}/packet", get(admin::packet))
        .route("/admin/traqula", post(admin::traqula))
        .route("/metrics", get(metrics))
        .route("/healthz", get(|| async { "ok" }))
        .layer(axum::extract::DefaultBodyLimit::max(state.config.max_body_bytes))
        .with_state(state)
}

/// Background housekeeping: evict idle sessions from memory once a minute, and
/// poll the next hop's `/v1/models` for weight swaps.
pub fn start_background(state: &Shared) {
    if state.config.models_poll_secs > 0 {
        let st = state.clone();
        let every = std::time::Duration::from_secs(st.config.models_poll_secs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tick.tick().await;
                if let Err(e) = pipeline::poll_models(&st).await {
                    tracing::debug!("models poll: {e}");
                }
            }
        });
    }
    let st = state.clone();
    let idle = std::time::Duration::from_secs(st.config.session_idle_secs);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            let n = pipeline::evict_idle(&st, idle);
            if n > 0 {
                tracing::info!(evicted = n, "idle sessions evicted from memory");
            }
        }
    });
}

/// Bind and serve in the background (tests, embedding). Returns the address.
pub async fn spawn(config: Config) -> anyhow::Result<(SocketAddr, Shared)> {
    let state = build(config)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = router(state.clone());
    start_background(&state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((addr, state))
}

fn into_response(u: Upstreamed) -> Response {
    match u {
        Upstreamed::Stream { headers, rx } => (StatusCode::OK, headers, Body::from_stream(ReceiverStream::new(rx))).into_response(),
        Upstreamed::Json { status, headers, body } => (status, headers, axum::Json(body)).into_response(),
        Upstreamed::Raw { status, headers, body } => (status, headers, body).into_response(),
    }
}

#[allow(clippy::result_large_err)] // the error is the response itself
fn ingress(
    state: &Shared,
    headers: &HeaderMap,
    body: Value,
    raw: Option<Bytes>,
    native: Option<String>,
    default_profile: &str,
) -> Result<Ingress, Response> {
    let developer = state.developer_for(bearer(headers).as_deref()).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "unknown token"))?;
    let profile_name = headers.get("x-handover-harness").and_then(|v| v.to_str().ok()).unwrap_or(default_profile).to_string();
    let profile = state.profile(&profile_name);
    let conv = Conversation::from_chat(handover_protocol::chat::messages(&body), &profile);
    let (client_key, weak) = pipeline::session_key(headers, &profile, native, &developer, &conv);
    Ok(Ingress {
        session_key: state.namespaced(&developer, &client_key),
        client_key,
        developer,
        weak,
        profile,
        body,
        raw,
        client_auth: headers.get("authorization").cloned(),
        forward_auth: state.config.upstream().forward_client_auth && state.config.upstream().api_key.is_none(),
    })
}

async fn chat_completions(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(json) = serde_json::from_slice::<Value>(&body) else { return err(StatusCode::BAD_REQUEST, "body is not JSON") };
    let default = state.config.default_profile.clone();
    match ingress(&state, &headers, json, Some(body), None, &default) {
        Ok(ing) => into_response(pipeline::run(state, ing).await),
        Err(r) => r,
    }
}

/// Anthropic Messages ingress: translate to Chat Completions, run the same
/// pipeline, translate the answer back (§13).
async fn anthropic_messages(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<Value>(&body) else { return err(StatusCode::BAD_REQUEST, "body is not JSON") };
    let chat_body = match messages::request_to_chat(&req) {
        Ok(b) => b,
        Err(e) => return anthropic_error(StatusCode::BAD_REQUEST, &e),
    };
    let model = req.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let native = messages::session_from_metadata(&req);
    let ing = match ingress(&state, &headers, chat_body, None, native, "claude-code") {
        Ok(i) => i,
        Err(r) => return r,
    };
    match pipeline::run(state, ing).await {
        Upstreamed::Stream { headers, mut rx } => {
            let (tx, out_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
            tokio::spawn(async move {
                let mut parser = SseParser::new();
                let mut tr = messages::StreamTranslator::new(&model);
                while let Some(item) = rx.recv().await {
                    let Ok(bytes) = item else { break };
                    for ev in parser.push(&bytes) {
                        if ev.data.trim() == "[DONE]" {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                            for b in tr.chunk(&v) {
                                if tx.send(Ok(b)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                for b in tr.finish() {
                    let _ = tx.send(Ok(b)).await;
                }
            });
            (StatusCode::OK, headers, Body::from_stream(ReceiverStream::new(out_rx))).into_response()
        }
        Upstreamed::Json { status, headers, body } if status.is_success() => {
            (status, headers, axum::Json(messages::completion_to_message(&body))).into_response()
        }
        Upstreamed::Json { status, body, .. } => {
            anthropic_error(status, body.pointer("/error/message").and_then(Value::as_str).unwrap_or("error"))
        }
        Upstreamed::Raw { status, body, .. } => anthropic_error(status, &String::from_utf8_lossy(&body)),
    }
}

/// OpenAI Responses ingress (Codex, `store: false`): translate to Chat
/// Completions, run the same pipeline, translate the answer back (§13).
async fn openai_responses(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<Value>(&body) else { return err(StatusCode::BAD_REQUEST, "body is not JSON") };
    let chat_body = match responses::request_to_chat(&req) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let model = req.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    // Codex sends its conversation id as the prompt cache key.
    let native = req.get("prompt_cache_key").and_then(Value::as_str).map(str::to_string);
    let ing = match ingress(&state, &headers, chat_body, None, native, "codex") {
        Ok(i) => i,
        Err(r) => return r,
    };
    match pipeline::run(state, ing).await {
        Upstreamed::Stream { headers, mut rx } => {
            let (tx, out_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
            tokio::spawn(async move {
                let mut parser = SseParser::new();
                let mut tr = responses::StreamTranslator::new(&model);
                while let Some(item) = rx.recv().await {
                    let Ok(bytes) = item else { break };
                    for ev in parser.push(&bytes) {
                        if ev.data.trim() == "[DONE]" {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                            for b in tr.chunk(&v) {
                                if tx.send(Ok(b)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                for b in tr.finish() {
                    let _ = tx.send(Ok(b)).await;
                }
            });
            (StatusCode::OK, headers, Body::from_stream(ReceiverStream::new(out_rx))).into_response()
        }
        Upstreamed::Json { status, headers, body } if status.is_success() => {
            (status, headers, axum::Json(responses::completion_to_response(&body))).into_response()
        }
        other => into_response(other),
    }
}

fn anthropic_error(status: StatusCode, msg: &str) -> Response {
    let kind = match status.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        529 | 503 => "overloaded_error",
        _ => "api_error",
    };
    (status, axum::Json(json!({"type": "error", "error": {"type": kind, "message": msg}}))).into_response()
}

async fn models(State(state): State<Shared>) -> Response {
    let up = state.config.upstream();
    let mut req = state.http.get(format!("{}/v1/models", up.base_url.trim_end_matches('/')));
    if let Some(k) = &up.api_key {
        req = req.bearer_auth(k);
    }
    match req.send().await {
        Ok(r) => {
            let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = r.bytes().await.unwrap_or_default();
            (status, [("content-type", "application/json")], body).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

async fn metrics(State(state): State<Shared>) -> Response {
    ([("content-type", "text/plain; version=0.0.4")], state.metrics.render()).into_response()
}
