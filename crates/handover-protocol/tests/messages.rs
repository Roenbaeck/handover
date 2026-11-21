use bytes::Bytes;
use handover_protocol::messages::*;
use serde_json::{Value, json};

/// Parse produced SSE blobs back into (event name, data JSON) pairs, checking
/// that each blob is exactly one event whose name equals its `type`.
fn events(blobs: &[Bytes]) -> Vec<(String, Value)> {
    blobs
        .iter()
        .map(|b| {
            let text = std::str::from_utf8(b).expect("utf-8");
            let body = text.strip_suffix("\n\n").expect("terminated by a blank line");
            let (event_line, data_line) = body.split_once('\n').expect("two lines");
            let event = event_line.strip_prefix("event: ").expect("event line").to_string();
            let data: Value = serde_json::from_str(data_line.strip_prefix("data: ").expect("data line")).expect("json");
            assert_eq!(data["type"], event.as_str(), "event name equals type");
            (event, data)
        })
        .collect()
}

fn types(evs: &[(String, Value)]) -> Vec<&str> {
    evs.iter().map(|(e, _)| e.as_str()).collect()
}

#[test]
fn claude_code_request_to_chat() {
    let req = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 32000,
        "temperature": 1,
        "top_k": 5,
        "stream": true,
        "stop_sequences": ["</done>"],
        "metadata": {"user_id": "user_abc_account_def_session_ghi"},
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "system": [
            {"type": "text", "text": "You are Claude Code.", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "Be concise."}
        ],
        "messages": [
            {"role": "user", "content": "List the files."},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "I should run ls.", "signature": "sig"},
                {"type": "text", "text": "Let me look."},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}},
                {"type": "tool_use", "id": "toolu_2", "name": "Read", "input": {"file_path": "/a"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a\nb"},
                {"type": "tool_result", "tool_use_id": "toolu_2", "is_error": true,
                 "content": [{"type": "text", "text": "no such file"}]},
                {"type": "text", "text": "<system-reminder>todo</system-reminder>"},
                {"type": "text", "text": "Thanks.", "cache_control": {"type": "ephemeral"}}
            ]}
        ],
        "tools": [
            {"name": "Bash", "description": "Run a command",
             "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}},
            {"type": "web_search_20250305", "name": "web_search", "max_uses": 5}
        ],
        "tool_choice": {"type": "auto"}
    });

    let chat = request_to_chat(&req).unwrap();
    assert_eq!(
        chat,
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 32000,
            "temperature": 1,
            "stop": ["</done>"],
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [
                {"role": "system", "content": "You are Claude Code.\n\nBe concise."},
                {"role": "user", "content": "List the files."},
                {"role": "assistant", "content": "Let me look.", "tool_calls": [
                    {"id": "toolu_1", "type": "function",
                     "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}},
                    {"id": "toolu_2", "type": "function",
                     "function": {"name": "Read", "arguments": "{\"file_path\":\"/a\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "toolu_1", "content": "a\nb"},
                {"role": "tool", "tool_call_id": "toolu_2", "content": "Error: no such file"},
                {"role": "user", "content": [{"type": "text", "text": "<system-reminder>todo</system-reminder>"}, {"type": "text", "text": "Thanks."}]}
            ],
            "tools": [
                {"type": "function", "function": {
                    "name": "Bash", "description": "Run a command",
                    "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}
                }}
            ],
            "tool_choice": "auto"
        })
    );
}

#[test]
fn request_edge_cases() {
    assert!(request_to_chat(&json!({"model": "m"})).is_err());
    assert!(request_to_chat(&json!({"messages": "hi"})).is_err());
    assert!(request_to_chat(&json!("not an object")).is_err());

    let chat = request_to_chat(&json!({
        "system": "Plain system.",
        "messages": [
            // Only tool results: no trailing user message.
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1"}
            ]},
            // Tool use only: content is null.
            {"role": "assistant", "content": [
                {"type": "redacted_thinking", "data": "x"},
                {"type": "tool_use", "id": "t2", "name": "Glob", "input": {}}
            ]},
            // Images make the content an array of parts, in order.
            {"role": "user", "content": [
                {"type": "text", "text": "What is this?"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}},
                {"type": "text", "text": "Briefly."}
            ]}
        ],
        "tools": [{"name": "Glob", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "tool", "name": "Glob", "disable_parallel_tool_use": true}
    }))
    .unwrap();
    assert_eq!(chat["messages"][0], json!({"role": "system", "content": "Plain system."}));
    assert_eq!(chat["messages"][1], json!({"role": "tool", "tool_call_id": "t1", "content": ""}));
    assert_eq!(chat["messages"][2]["content"], Value::Null);
    assert_eq!(chat["messages"][2]["tool_calls"][0]["function"]["arguments"], "{}");
    assert_eq!(
        chat["messages"][3],
        json!({"role": "user", "content": [
            {"type": "text", "text": "What is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            {"type": "text", "text": "Briefly."}
        ]})
    );
    assert_eq!(chat["messages"].as_array().unwrap().len(), 4);
    assert_eq!(chat["tool_choice"], json!({"type": "function", "function": {"name": "Glob"}}));
    assert_eq!(chat["parallel_tool_calls"], false);
    assert!(chat.get("stream").is_none());

    for (choice, expected) in [(json!({"type": "any"}), json!("required")), (json!({"type": "none"}), json!("none"))] {
        let chat = request_to_chat(&json!({
            "messages": [],
            "tools": [{"name": "X", "input_schema": {"type": "object"}}],
            "tool_choice": choice
        }))
        .unwrap();
        assert_eq!(chat["tool_choice"], expected);
    }
}

#[test]
fn completion_to_message_maps_tools_and_usage() {
    let chat = json!({
        "id": "chatcmpl-123",
        "object": "chat.completion",
        "model": "qwen3-coder",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Running it.",
                "reasoning_content": "hidden",
                "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}},
                    {"id": "call_2", "type": "function",
                     "function": {"name": "Read", "arguments": "{not json"}}
                ]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 800}
        }
    });
    assert_eq!(
        completion_to_message(&chat),
        json!({
            "id": "msg_chatcmpl-123",
            "type": "message",
            "role": "assistant",
            "model": "qwen3-coder",
            "content": [
                {"type": "text", "text": "Running it."},
                {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {"command": "ls"}},
                {"type": "tool_use", "id": "call_2", "name": "Read", "input": {}}
            ],
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {"input_tokens": 200, "output_tokens": 50, "cache_read_input_tokens": 800}
        })
    );

    let plain = completion_to_message(&json!({
        "id": "x", "model": "m",
        "choices": [{"message": {"content": ""}, "finish_reason": "length"}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3}
    }));
    assert_eq!(plain["content"], json!([]));
    assert_eq!(plain["stop_reason"], "max_tokens");
    assert_eq!(plain["usage"], json!({"input_tokens": 7, "output_tokens": 3, "cache_read_input_tokens": 0}));

    for (finish, expected) in [(json!("stop"), json!("end_turn")), (json!("content_filter"), json!("end_turn")), (Value::Null, Value::Null)]
    {
        let m = completion_to_message(&json!({"choices": [{"message": {}, "finish_reason": finish}]}));
        assert_eq!(m["stop_reason"], expected);
    }
    // Garbage in, well-formed message out.
    assert_eq!(completion_to_message(&json!(42))["content"], json!([]));
}

fn chunk(delta: Value, finish: Value) -> Value {
    json!({
        "id": "chatcmpl-9",
        "object": "chat.completion.chunk",
        "model": "upstream-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
    })
}

#[test]
fn stream_text_then_two_tools_then_usage() {
    let mut t = StreamTranslator::new("claude-sonnet-4-5");
    let mut blobs = Vec::new();
    let chunks = [
        chunk(json!({"role": "assistant", "content": ""}), Value::Null),
        chunk(json!({"reasoning_content": "thinking..."}), Value::Null),
        chunk(json!({"content": "Let me "}), Value::Null),
        chunk(json!({"content": "check."}), Value::Null),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
                                   "function": {"name": "Bash", "arguments": ""}}]}),
            Value::Null,
        ),
        chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"command\":"}}]}), Value::Null),
        chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"ls\"}"}}]}), Value::Null),
        chunk(
            json!({"tool_calls": [{"index": 1, "id": "call_b", "type": "function",
                                   "function": {"name": "Read", "arguments": "{\"file_path\":\"/a\"}"}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        json!({
            "id": "chatcmpl-9", "object": "chat.completion.chunk", "choices": [],
            "usage": {"prompt_tokens": 120, "completion_tokens": 30,
                      "prompt_tokens_details": {"cached_tokens": 100}}
        }),
    ];
    for c in &chunks {
        blobs.extend(t.chunk(c));
    }
    blobs.extend(t.finish());
    assert!(t.finish().is_empty(), "finish emits exactly once");
    assert!(t.chunk(&chunks[2]).is_empty(), "nothing after message_stop");

    let evs = events(&blobs);
    assert_eq!(
        types(&evs),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );

    assert_eq!(
        evs[0].1,
        json!({"type": "message_start", "message": {
            "id": "msg_chatcmpl-9", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5", "content": [], "stop_reason": null,
            "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0}
        }})
    );
    assert_eq!(evs[1].1, json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}));
    assert_eq!(evs[2].1["delta"], json!({"type": "text_delta", "text": "Let me "}));
    assert_eq!(evs[3].1["delta"]["text"], "check.");
    assert_eq!(evs[4].1, json!({"type": "content_block_stop", "index": 0}));
    assert_eq!(
        evs[5].1,
        json!({"type": "content_block_start", "index": 1,
               "content_block": {"type": "tool_use", "id": "call_a", "name": "Bash", "input": {}}})
    );
    assert_eq!(
        evs[6].1,
        json!({"type": "content_block_delta", "index": 1,
               "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}})
    );
    assert_eq!(evs[7].1["delta"]["partial_json"], "\"ls\"}");
    assert_eq!(evs[8].1["index"], 1);
    assert_eq!(evs[9].1["index"], 2);
    assert_eq!(evs[9].1["content_block"]["id"], "call_b");
    assert_eq!(evs[9].1["content_block"]["name"], "Read");
    assert_eq!(evs[10].1["index"], 2);
    assert_eq!(evs[10].1["delta"]["partial_json"], "{\"file_path\":\"/a\"}");
    assert_eq!(evs[11].1, json!({"type": "content_block_stop", "index": 2}));
    assert_eq!(evs[12].1["delta"], json!({"stop_reason": "tool_use", "stop_sequence": null}));
    assert_eq!(evs[12].1["usage"]["output_tokens"], 30);
    assert_eq!(evs[12].1["usage"]["input_tokens"], 20);
    assert_eq!(evs[12].1["usage"]["cache_read_input_tokens"], 100);
    assert_eq!(evs[13].1, json!({"type": "message_stop"}));

    // Reassembled tool input is valid JSON.
    let input: String = evs[6..8].iter().map(|(_, d)| d["delta"]["partial_json"].as_str().unwrap()).collect();
    assert_eq!(serde_json::from_str::<Value>(&input).unwrap(), json!({"command": "ls"}));
}

#[test]
fn stream_edge_cases() {
    // No chunks at all: still a well-formed message.
    let mut t = StreamTranslator::new("m");
    let evs = events(&t.finish());
    assert_eq!(types(&evs), ["message_start", "message_delta", "message_stop"]);
    assert_eq!(evs[1].1["delta"]["stop_reason"], Value::Null);
    assert_eq!(evs[1].1["usage"], json!({"output_tokens": 0}));
    assert!(t.finish().is_empty());

    // Truncated stream: finish closes the open block; text after a tool call
    // opens a new block; ids repeated on every fragment do not start new calls.
    let mut t = StreamTranslator::new("m");
    let mut blobs = Vec::new();
    blobs.extend(
        t.chunk(&chunk(json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "A", "arguments": "{"}}]}), Value::Null)),
    );
    blobs.extend(t.chunk(&chunk(json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"arguments": "}"}}]}), Value::Null)));
    blobs.extend(t.chunk(&chunk(json!({"content": "done"}), Value::Null)));
    blobs.extend(t.chunk(&json!({"garbage": true})));
    blobs.extend(t.chunk(&json!(null)));
    blobs.extend(t.finish());
    let evs = events(&blobs);
    assert_eq!(
        types(&evs),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    assert_eq!(evs[1].1["content_block"]["id"], "c1");
    assert_eq!(evs[5].1["index"], 1);
    assert_eq!(evs[5].1["content_block"]["type"], "text");
    assert_eq!(evs[7].1["index"], 1);
    assert_eq!(evs[8].1["delta"]["stop_reason"], Value::Null);

    // Backends that reuse index 0 with a fresh id per call.
    let mut t = StreamTranslator::new("m");
    let mut blobs = Vec::new();
    for id in ["x1", "x2"] {
        blobs.extend(
            t.chunk(&chunk(json!({"tool_calls": [{"index": 0, "id": id, "function": {"name": "T", "arguments": "{}"}}]}), Value::Null)),
        );
    }
    blobs.extend(t.chunk(&chunk(json!({}), json!("stop"))));
    blobs.extend(t.finish());
    let evs = events(&blobs);
    let starts: Vec<(u64, &str)> = evs
        .iter()
        .filter(|(e, _)| e == "content_block_start")
        .map(|(_, d)| (d["index"].as_u64().unwrap(), d["content_block"]["id"].as_str().unwrap()))
        .collect();
    assert_eq!(starts, [(0, "x1"), (1, "x2")]);
    assert_eq!(evs.last().unwrap().0, "message_stop");
    assert_eq!(evs[evs.len() - 2].1["delta"]["stop_reason"], "end_turn");
}

#[test]
fn session_ids() {
    let req = |user_id: Value| json!({"metadata": {"user_id": user_id}, "messages": []});
    assert_eq!(
        session_from_metadata(&req(json!(
            "user_0123abcd_account_6f1c2b9e-0000-4000-8000-000000000001_session_8a7b6c5d-1111-4222-8333-444455556666"
        ))),
        Some("8a7b6c5d-1111-4222-8333-444455556666".to_string())
    );
    // API-key users have an empty account part.
    assert_eq!(session_from_metadata(&req(json!("user_abc_account__session_s-1"))), Some("s-1".to_string()));
    // JSON-encoded form used by newer Claude Code versions.
    assert_eq!(
        session_from_metadata(&req(json!("{\"device_id\":\"d\",\"account_uuid\":\"a\",\"session_id\":\"5e55-10\"}"))),
        Some("5e55-10".to_string())
    );
    assert_eq!(session_from_metadata(&req(json!("user_abc_session_x"))), None);
    assert_eq!(session_from_metadata(&req(json!("user_abc_account_def_session_"))), None);
    assert_eq!(session_from_metadata(&req(json!("someone"))), None);
    assert_eq!(session_from_metadata(&req(json!(7))), None);
    assert_eq!(session_from_metadata(&json!({"messages": []})), None);
}
