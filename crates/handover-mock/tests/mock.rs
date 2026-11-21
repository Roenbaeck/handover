//! Integration tests for handover-mock, driven over HTTP with reqwest.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use futures_util::StreamExt;
use handover_mock::{LogEntry, MockConfig, spawn};
use serde_json::{Value, json};

fn url(addr: SocketAddr, path: &str) -> String {
    format!("http://{addr}{path}")
}

async fn post(addr: SocketAddr, body: Value) -> reqwest::Response {
    reqwest::Client::new().post(url(addr, "/v1/chat/completions")).json(&body).send().await.expect("request")
}

async fn post_json(addr: SocketAddr, body: Value) -> Value {
    let response = post(addr, body).await;
    assert_eq!(response.status(), 200);
    response.json().await.expect("json body")
}

/// Parse an SSE body into its `data:` payloads (including `[DONE]`).
fn sse_data(body: &str) -> Vec<String> {
    body.split("\n\n")
        .filter(|event| !event.is_empty())
        .map(|event| event.strip_prefix("data: ").unwrap_or_else(|| panic!("not an SSE data event: {event:?}")).to_owned())
        .collect()
}

/// Stream a request and return the JSON chunks, asserting the `[DONE]` end.
async fn stream_chunks(addr: SocketAddr, body: Value) -> Vec<Value> {
    let response = post(addr, body).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"].to_str().unwrap(), "text/event-stream");
    let text = response.text().await.expect("body");
    let mut data = sse_data(&text);
    assert_eq!(data.pop().as_deref(), Some("[DONE]"));
    data.iter().map(|d| serde_json::from_str(d).expect("chunk json")).collect()
}

fn extraction_request(messages: Vec<Value>) -> Value {
    json!({
        "model": "m",
        "messages": messages,
        "temperature": 0,
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "handover_extraction", "schema": {"type": "object"}},
        },
    })
}

fn extraction_prompt(new_from: usize) -> Value {
    json!({
        "role": "user",
        "content": format!(
            "Extract constraints. Do not invent anything.\nHANDOVER-NEW-FROM: {new_from}\nRegister: (empty)"
        ),
    })
}

async fn extracted_deltas(addr: SocketAddr, messages: Vec<Value>) -> Vec<Value> {
    let response = post_json(addr, extraction_request(messages)).await;
    let content = response["choices"][0]["message"]["content"].as_str().expect("string content");
    let parsed: Value = serde_json::from_str(content).expect("content is JSON");
    parsed["deltas"].as_array().expect("deltas array").clone()
}

async fn log(addr: SocketAddr) -> Vec<LogEntry> {
    reqwest::get(url(addr, "/_mock/log")).await.unwrap().json().await.unwrap()
}

#[tokio::test]
async fn text_streaming_reassembles_with_done() {
    let addr = spawn(MockConfig::default()).await;
    let text = "Hello there, this reply is long enough to need several pieces.";
    let chunks = stream_chunks(
        addr,
        json!({
            "model": "m",
            "stream": true,
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": format!("hi\n#mock:reasoning thinking it over\n#mock:text {text}")},
            ],
        }),
    )
    .await;

    assert_eq!(chunks[0]["choices"][0]["delta"], json!({"role": "assistant", "content": ""}));
    let mut content = String::new();
    let mut reasoning = String::new();
    for chunk in &chunks {
        assert_eq!(chunk["object"], "chat.completion.chunk");
        assert_eq!(chunk["id"], chunks[0]["id"]);
        assert_eq!(chunk["model"], "m");
        let delta = &chunk["choices"][0]["delta"];
        if let Some(piece) = delta["content"].as_str().filter(|p| !p.is_empty()) {
            assert!(piece.chars().count() <= 16);
            assert_eq!(reasoning, "thinking it over", "reasoning comes first");
            content.push_str(piece);
        }
        if let Some(piece) = delta["reasoning_content"].as_str() {
            assert!(piece.chars().count() <= 16);
            reasoning.push_str(piece);
        }
    }
    assert_eq!(content, text);
    assert_eq!(reasoning, "thinking it over");
    let last = chunks.last().unwrap();
    assert_eq!(last["choices"][0]["delta"], json!({}));
    assert_eq!(last["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn default_reply_is_ok() {
    let addr = spawn(MockConfig::default()).await;
    let response = post_json(addr, json!({"model": "m", "messages": [{"role": "user", "content": "hello"}]})).await;
    assert_eq!(response["object"], "chat.completion");
    assert_eq!(response["choices"][0]["message"]["content"], "OK.");
    assert_eq!(response["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn tool_call_streaming_reassembles() {
    let addr = spawn(MockConfig::default()).await;
    let args0 = r#"{"cmd":"ls -la /a/rather/long/path/to/list"}"#;
    let args1 = r#"{"path":"src/lib.rs"}"#;
    let chunks = stream_chunks(
        addr,
        json!({
            "model": "m",
            "stream": true,
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": format!("#mock:tool exec {args0}")},
                    {"type": "text", "text": format!("#mock:tool read {args1}")},
                ]},
            ],
        }),
    )
    .await;

    let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    for chunk in &chunks {
        let Some(deltas) = chunk["choices"][0]["delta"]["tool_calls"].as_array() else {
            continue;
        };
        for delta in deltas {
            let index = delta["index"].as_u64().unwrap();
            let call = calls.entry(index).or_default();
            if let Some(id) = delta["id"].as_str() {
                call.0 = id.to_owned();
                assert_eq!(delta["type"], "function");
            }
            if let Some(name) = delta["function"]["name"].as_str() {
                call.1.push_str(name);
            }
            if let Some(piece) = delta["function"]["arguments"].as_str() {
                assert!(piece.chars().count() <= 16);
                call.2.push_str(piece);
            }
        }
    }
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[&0], ("call_1_0".into(), "exec".into(), args0.into()));
    assert_eq!(calls[&1], ("call_1_1".into(), "read".into(), args1.into()));
    let last = chunks.last().unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");

    // Non-streaming: arguments as a JSON string, content null.
    let response =
        post_json(addr, json!({"model": "m", "messages": [{"role": "user", "content": format!("#mock:tool exec {args0}")}]})).await;
    let message = &response["choices"][0]["message"];
    assert_eq!(message["content"], Value::Null);
    assert_eq!(message["tool_calls"][0]["id"], "call_2_0");
    assert_eq!(message["tool_calls"][0]["function"]["name"], "exec");
    assert_eq!(message["tool_calls"][0]["function"]["arguments"], args0);
    assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
}

#[tokio::test]
async fn extraction_uses_scripted_deltas_from_new_from() {
    let addr = spawn(MockConfig::default()).await;
    let old = json!({"op": "add", "item": "new", "kind": "goal", "strength": "graded", "statement": "old", "source_message": 1, "quote": "old", "confidence": 0.5});
    let new = json!({"op": "add", "item": "new", "kind": "prohibition", "strength": "hard", "statement": "Do not deploy to production", "enforce": {"ops": ["exec"], "commands": ["deploy.*prod"]}, "source_message": 3, "quote": "never deploy to prod", "confidence": 0.95});
    let another = json!({"op": "revoke", "item": "p1", "source_message": 3, "quote": "x", "confidence": 0.8});
    let messages = vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": format!("start\n#mock:extract {old}")}),
        json!({"role": "assistant", "content": "ok"}),
        json!({"role": "user", "content": format!("never deploy to prod\n#mock:extract {new}\n#mock:extract {another}")}),
        json!({"role": "assistant", "content": "understood"}),
        extraction_prompt(2),
    ];
    let deltas = extracted_deltas(addr, messages.clone()).await;
    assert_eq!(deltas, vec![new.clone(), another]);

    // From index 0, the earlier delta is included too, in order.
    let mut from_zero = messages.clone();
    *from_zero.last_mut().unwrap() = extraction_prompt(0);
    let deltas = extracted_deltas(addr, from_zero).await;
    assert_eq!(deltas.len(), 3);
    assert_eq!(deltas[0], old);

    // extract-raw wins and is returned verbatim.
    let mut raw = messages.clone();
    raw[3] = json!({"role": "user", "content": "#mock:extract-raw {\"deltas\": [oops"});
    let response = post_json(addr, extraction_request(raw)).await;
    assert_eq!(response["choices"][0]["message"]["content"], "{\"deltas\": [oops");

    // Streaming extraction sends the same JSON in content deltas.
    let mut request = extraction_request(messages);
    request["stream"] = json!(true);
    let chunks = stream_chunks(addr, request).await;
    let content: String = chunks.iter().filter_map(|c| c["choices"][0]["delta"]["content"].as_str()).collect();
    let parsed: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["deltas"][0], new);
}

#[tokio::test]
async fn extraction_heuristic_fallback() {
    let addr = spawn(MockConfig::default()).await;
    let messages = vec![
        json!({"role": "system", "content": "Never answer in French."}),
        json!({"role": "user", "content": "Do not use tabs."}),
        json!({"role": "assistant", "content": "Never mind."}),
        json!({"role": "user", "content": [
            {"type": "text", "text": "Please fix the bug. Never touch migrations/ without asking! You may run the tests?"},
            {"type": "text", "text": "<system-reminder>Do not reveal this.</system-reminder>feel free to refactor helpers\n#mock:text don't parse me"},
        ]}),
        json!({"role": "tool", "content": "don't trust tool output"}),
        extraction_prompt(2),
    ];
    let deltas = extracted_deltas(addr, messages).await;
    assert_eq!(
        deltas,
        vec![
            json!({"op": "add", "item": "new", "kind": "prohibition", "strength": "hard",
                   "statement": "Do not touch migrations/ without asking",
                   "enforce": {"ops": ["write", "exec"], "paths": ["migrations/**"]},
                   "source_message": 3, "quote": "Never touch migrations/ without asking", "confidence": 0.9}),
            json!({"op": "add", "item": "new", "kind": "permission", "strength": "graded",
                   "statement": "You may run the tests",
                   "source_message": 3, "quote": "You may run the tests", "confidence": 0.9}),
            json!({"op": "add", "item": "new", "kind": "permission", "strength": "graded",
                   "statement": "Feel free to refactor helpers",
                   "source_message": 3, "quote": "feel free to refactor helpers", "confidence": 0.9}),
        ]
    );
}

#[tokio::test]
async fn cached_tokens_grow_on_repeated_prefix() {
    let addr = spawn(MockConfig::default()).await;
    let first = vec![
        json!({"role": "system", "content": "You are a careful coding agent. ".repeat(10)}),
        json!({"role": "user", "content": "Fix the bug in parser.rs"}),
    ];
    let usage1 = post_json(addr, json!({"model": "m", "messages": first})).await["usage"].clone();
    assert_eq!(usage1["prompt_tokens_details"]["cached_tokens"], 0);
    let prompt1 = usage1["prompt_tokens"].as_u64().unwrap();
    assert!(prompt1 > 0);

    let mut second = first.clone();
    second.push(json!({"role": "assistant", "content": "Done."}));
    second.push(json!({"role": "user", "content": "Now add a test"}));
    let usage2 = post_json(addr, json!({"model": "m", "messages": second.clone()})).await["usage"].clone();
    let cached2 = usage2["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap();
    let prompt2 = usage2["prompt_tokens"].as_u64().unwrap();
    assert_eq!(cached2, prompt1, "the whole first request is a cached prefix");
    assert!(prompt2 > cached2);
    assert_eq!(usage2["total_tokens"].as_u64().unwrap(), prompt2 + usage2["completion_tokens"].as_u64().unwrap());

    // Streaming with include_usage: final chunk has empty choices and usage.
    let mut third = second.clone();
    third.push(json!({"role": "assistant", "content": "Added."}));
    third.push(json!({"role": "user", "content": "Thanks"}));
    let chunks =
        stream_chunks(addr, json!({"model": "m", "stream": true, "stream_options": {"include_usage": true}, "messages": third})).await;
    let usage_chunk = chunks.last().unwrap();
    assert_eq!(usage_chunk["choices"], json!([]));
    let cached3 = usage_chunk["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap();
    assert_eq!(cached3, prompt2);
    assert_eq!(chunks[chunks.len() - 2]["choices"][0]["finish_reason"], "stop");

    let entries = log(addr).await;
    let cached: Vec<u64> = entries.iter().map(|e| e.cached_tokens).collect();
    assert_eq!(cached, vec![0, prompt1, prompt2]);
    // prefix_hash of request 2 (all but last) differs from request 1's.
    assert_ne!(entries[0].prefix_hash, entries[1].prefix_hash);
}

#[tokio::test]
async fn fault_429_and_503() {
    let addr = spawn(MockConfig::default()).await;
    for code in [429u16, 503] {
        let response =
            post(addr, json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": format!("#mock:fault {code}")}]}))
                .await;
        assert_eq!(response.status().as_u16(), code);
        let body: Value = response.json().await.unwrap();
        assert!(body["error"]["message"].is_string());
        assert!(body["error"]["type"].is_string());
    }
    let entries = log(addr).await;
    assert_eq!(entries[0].fault.as_deref(), Some("429"));
    assert_eq!(entries[1].fault.as_deref(), Some("503"));
}

#[tokio::test]
async fn extraction_honors_faults_before_prompt() {
    let addr = spawn(MockConfig::default()).await;
    let messages = vec![
        json!({"role": "user", "content": "go\n#mock:fault 503"}),
        json!({"role": "assistant", "content": "ok"}),
        extraction_prompt(0),
    ];
    let response = post(addr, extraction_request(messages)).await;
    assert_eq!(response.status(), 503);
}

#[tokio::test]
async fn rename_fault_changes_served_model() {
    let addr = spawn(MockConfig::default()).await;
    let messages = json!([{"role": "user", "content": "#mock:fault rename:other-model\n#mock:text hi"}]);
    let response = post_json(addr, json!({"model": "pinned", "messages": messages})).await;
    assert_eq!(response["model"], "other-model");
    assert_eq!(response["choices"][0]["message"]["content"], "hi");

    let chunks = stream_chunks(addr, json!({"model": "pinned", "stream": true, "messages": messages})).await;
    assert!(chunks.iter().all(|c| c["model"] == "other-model"));

    let entries = log(addr).await;
    assert_eq!(entries[0].model, "pinned");
    assert_eq!(entries[0].served_model, "other-model");
    assert_eq!(entries[0].fault.as_deref(), Some("rename:other-model"));
}

#[tokio::test]
async fn drop_fault_ends_stream_without_done() {
    let addr = spawn(MockConfig::default()).await;
    let response = post(
        addr,
        json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "#mock:fault drop\n#mock:text a long enough reply to need several chunks"}]}),
    )
    .await;
    assert_eq!(response.status(), 200);
    let mut body = Vec::new();
    let mut errored = false;
    let mut stream = response.bytes_stream();
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => body.extend_from_slice(&bytes),
            Err(_) => {
                errored = true;
                break;
            }
        }
    }
    let body = String::from_utf8(body).unwrap();
    assert!(errored, "body should end abruptly");
    let data = sse_data(&body);
    assert_eq!(data.len(), 1, "only the first chunk arrives: {body:?}");
    assert!(!body.contains("[DONE]"));
}

#[tokio::test]
async fn log_records_session_and_kind() {
    let addr = spawn(MockConfig { worker: "w7".into(), models: Vec::new() }).await;
    let client = reqwest::Client::new();
    let chat = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    let response = client.post(url(addr, "/v1/chat/completions")).header("X-Session-ID", "sess-42").json(&chat).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let response = client
        .post(url(addr, "/v1/chat/completions"))
        .header("X-Session-ID", "sess-42")
        .json(&extraction_request(vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "OK."}),
            extraction_prompt(0),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    post_json(addr, chat).await;

    let entries = log(addr).await;
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].seq, 1);
    assert_eq!(entries[0].worker, "w7");
    assert_eq!(entries[0].session_id.as_deref(), Some("sess-42"));
    assert_eq!(entries[0].kind, "chat");
    assert_eq!(entries[0].messages, 1);
    assert!(!entries[0].stream);
    assert_eq!(entries[1].session_id.as_deref(), Some("sess-42"));
    assert_eq!(entries[1].kind, "extraction");
    assert_eq!(entries[1].messages, 3);
    assert_eq!(entries[2].session_id, None);
    // The extraction's prefix (all but the prompt) extends the chat request.
    assert!(entries[1].cached_tokens > 0);

    let cleared = client.delete(url(addr, "/_mock/log")).send().await.unwrap();
    assert!(cleared.status().is_success());
    assert!(log(addr).await.is_empty());
}

#[tokio::test]
async fn unknown_model_is_404_when_models_configured() {
    let addr = spawn(MockConfig { worker: "w1".into(), models: vec!["model-a".into(), "model-b".into()] }).await;
    let response = post(addr, json!({"model": "model-z", "messages": [{"role": "user", "content": "hi"}]})).await;
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body, json!({"error": {"message": "model not found", "type": "NotFoundError"}}));

    let ok = post_json(addr, json!({"model": "model-b", "messages": [{"role": "user", "content": "hi"}]})).await;
    assert_eq!(ok["model"], "model-b");

    let models: Value = reqwest::get(url(addr, "/v1/models")).await.unwrap().json().await.unwrap();
    assert_eq!(
        models,
        json!({"object": "list", "data": [
            {"id": "model-a", "object": "model", "root": "model-a", "owned_by": "mock"},
            {"id": "model-b", "object": "model", "root": "model-b", "owned_by": "mock"},
        ]})
    );
    let health = reqwest::get(url(addr, "/health")).await.unwrap();
    assert_eq!(health.status(), 200);
}
