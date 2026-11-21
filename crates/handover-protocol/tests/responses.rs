use bytes::Bytes;
use handover_protocol::responses::*;
use serde_json::{Value, json};

/// Parse produced SSE blobs back into (event name, data JSON) pairs, checking
/// that each blob is exactly one event whose name equals its `type` and that
/// sequence numbers count up from 0.
fn events(blobs: &[Bytes]) -> Vec<(String, Value)> {
    blobs
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let text = std::str::from_utf8(b).expect("utf-8");
            let body = text.strip_suffix("\n\n").expect("terminated by a blank line");
            let (event_line, data_line) = body.split_once('\n').expect("two lines");
            let event = event_line.strip_prefix("event: ").expect("event line").to_string();
            let data: Value = serde_json::from_str(data_line.strip_prefix("data: ").expect("data line")).expect("json");
            assert_eq!(data["type"], event.as_str(), "event name equals type");
            assert_eq!(data["sequence_number"], i, "sequence number of event {i}");
            (event, data)
        })
        .collect()
}

fn types(evs: &[(String, Value)]) -> Vec<&str> {
    evs.iter().map(|(e, _)| e.as_str()).collect()
}

/// `serde_json::to_string(&{"action": action})`, as sent for a local shell call.
fn shell_args(action: Value) -> String {
    serde_json::to_string(&json!({"action": action})).unwrap()
}

#[test]
fn codex_request_to_chat() {
    let req = json!({
        "model": "gpt-5-codex",
        "instructions": "You are Codex.",
        "input": [
            {"type": "message", "role": "developer", "content": [
                {"type": "input_text", "text": "<permissions instructions>sandboxed</permissions instructions>"}
            ]},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "<environment_context><cwd>/repo</cwd></environment_context>"}
            ]},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "List the files"},
                {"type": "input_text", "text": "and read one."}
            ]},
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Plan"}],
             "encrypted_content": "gAAAA"},
            {"type": "message", "role": "assistant", "id": "msg_1", "content": [
                {"type": "output_text", "text": "Looking.", "annotations": []}
            ]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "shell",
             "arguments": "{\"command\":[\"ls\"]}"},
            {"type": "reasoning", "id": "rs_2", "summary": []},
            {"type": "function_call", "call_id": "call_2", "name": "read_file", "arguments": "{\"path\":\"a\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "a\nb"},
            {"type": "function_call_output", "call_id": "call_2", "output": [
                {"type": "input_text", "text": "line 1"},
                {"type": "input_text", "text": "line 2"}
            ]},
            {"type": "local_shell_call", "id": "lsh_9", "call_id": "call_3", "status": "completed",
             "action": {"type": "exec", "command": ["cat", "a"], "timeout_ms": 1000}},
            {"type": "local_shell_call_output", "call_id": "call_3", "output": "hello"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]},
            {"role": "user", "content": "Thanks"}
        ],
        "tools": [
            {"type": "function", "name": "shell", "description": "Runs a shell command", "strict": false,
             "parameters": {"type": "object", "properties": {"command": {"type": "array", "items": {"type": "string"}}},
                            "required": ["command"]}},
            {"type": "local_shell"},
            {"type": "web_search"},
            {"type": "function", "name": "read_file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "reasoning": {"effort": "medium", "summary": "auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "0199a1b2-c3d4",
        "text": {"verbosity": "medium"},
        "metadata": {"k": "v"},
        "temperature": 0.2,
        "top_p": 0.9,
        "max_output_tokens": 4096
    });

    let chat = request_to_chat(&req).unwrap();
    assert_eq!(
        chat,
        json!({
            "model": "gpt-5-codex",
            "temperature": 0.2,
            "top_p": 0.9,
            "max_tokens": 4096,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [
                // `instructions` and the developer message merge into one leading system message.
                {"role": "system", "content": "You are Codex.\n\n<permissions instructions>sandboxed</permissions instructions>"},
                {"role": "user", "content": "<environment_context><cwd>/repo</cwd></environment_context>"},
                {"role": "user", "content": [{"type": "text", "text": "List the files"}, {"type": "text", "text": "and read one."}]},
                {"role": "assistant", "content": "Looking.", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "shell", "arguments": "{\"command\":[\"ls\"]}"}},
                    {"id": "call_2", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "a\nb"},
                {"role": "tool", "tool_call_id": "call_2", "content": "line 1\n\nline 2"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_3", "type": "function", "function": {
                        "name": "local_shell",
                        "arguments": shell_args(json!({"type": "exec", "command": ["cat", "a"], "timeout_ms": 1000}))
                    }}
                ]},
                {"role": "tool", "tool_call_id": "call_3", "content": "hello"},
                {"role": "assistant", "content": "Done."},
                {"role": "user", "content": "Thanks"}
            ],
            "tools": [
                {"type": "function", "function": {
                    "name": "shell", "description": "Runs a shell command",
                    "parameters": {"type": "object", "properties": {"command": {"type": "array", "items": {"type": "string"}}},
                                   "required": ["command"]}
                }},
                {"type": "function", "function": {
                    "name": "local_shell",
                    "description": chat["tools"][1]["function"]["description"].clone(),
                    "parameters": {"type": "object", "properties": {"action": {"type": "object"}}}
                }},
                {"type": "function", "function": {
                    "name": "read_file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
                }}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": true
        })
    );
    assert!(chat["tools"][1]["function"]["description"].as_str().is_some_and(|d| d.contains("command")));
}

#[test]
fn request_errors() {
    let err = request_to_chat(&json!({"previous_response_id": "resp_1", "input": "hi"})).unwrap_err();
    assert!(err.contains("previous_response_id"), "{err}");
    assert!(request_to_chat(&json!({"model": "m"})).is_err());
    assert!(request_to_chat(&json!({"input": 7})).is_err());
    assert!(request_to_chat(&json!({"input": {"type": "message"}})).is_err());
    assert!(request_to_chat(&json!("not an object")).is_err());
    // A null or empty id is not a stateful request.
    assert!(request_to_chat(&json!({"previous_response_id": null, "input": "hi"})).is_ok());
    assert!(request_to_chat(&json!({"previous_response_id": "", "input": "hi"})).is_ok());
}

#[test]
fn string_input() {
    let chat = request_to_chat(&json!({
        "model": "m",
        "instructions": "Be brief.",
        "input": "Hello",
        "stream": false,
        "store": false,
        "max_output_tokens": null,
        "tool_choice": "required",
        "parallel_tool_calls": false
    }))
    .unwrap();
    // No tools: neither tool_choice nor parallel_tool_calls is forwarded.
    assert_eq!(
        chat,
        json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "Hello"}
            ]
        })
    );
}

#[test]
fn request_edge_cases() {
    let chat = request_to_chat(&json!({
        "input": [
            // Images make user content an array of parts, in order.
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "What is this?"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high"},
                {"type": "input_image", "image_url": {"url": "https://example.com/x.png"}},
                {"type": "input_image", "file_id": "file_1"},
                {"type": "input_file", "file_id": "file_2"},
                {"type": "input_text", "text": "Briefly."}
            ]},
            // Images are dropped outside user messages; empty messages vanish.
            {"type": "message", "role": "system", "content": [
                {"type": "input_image", "image_url": "data:image/png;base64,BBBB"},
                {"type": "text", "text": "Rules."}
            ]},
            {"type": "message", "role": "user", "content": []},
            {"type": "message", "role": "user", "content": ""},
            {"type": "message", "content": "no role"},
            {"type": "message", "role": "tool", "content": "odd role"},
            {"type": "message", "role": "assistant", "content": []},
            // Two assistant messages in a row stay separate; the second merges
            // with the following calls.
            {"type": "message", "role": "assistant", "content": "First."},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Second."}]},
            {"type": "function_call", "call_id": "c1", "name": "noargs", "arguments": ""},
            {"type": "local_shell_call", "id": "c2", "action": {"type": "exec", "command": ["pwd"]}},
            {"type": "web_search_call", "id": "ws_1", "status": "completed"},
            {"type": "function_call_output", "call_id": "c1", "output": {"ok": true}},
            {"type": "local_shell_call_output", "id": "c2", "output": null},
            {"type": "function_call", "call_id": "c3", "name": "late", "arguments": {"x": 1}},
            "garbage",
            42
        ],
        "tools": [
            {"type": "function", "name": "noargs"},
            {"type": "function", "description": "nameless"},
            {"type": "custom", "name": "apply_patch"},
            {"name": "untyped", "parameters": {"type": "object"}}
        ],
        "tool_choice": {"type": "function", "name": "noargs"}
    }))
    .unwrap();

    assert_eq!(
        chat["messages"],
        json!([
            {"role": "user", "content": [
                {"type": "text", "text": "What is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA", "detail": "high"}},
                {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}},
                {"type": "text", "text": "Briefly."}
            ]},
            {"role": "system", "content": "Rules."},
            {"role": "assistant", "content": "First."},
            {"role": "assistant", "content": "Second.", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "noargs", "arguments": "{}"}},
                {"id": "c2", "type": "function", "function": {
                    "name": "local_shell", "arguments": shell_args(json!({"type": "exec", "command": ["pwd"]}))
                }}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "{\"ok\":true}"},
            {"role": "tool", "tool_call_id": "c2", "content": ""},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c3", "type": "function", "function": {"name": "late", "arguments": "{\"x\":1}"}}
            ]}
        ])
    );
    assert_eq!(
        chat["tools"],
        json!([{"type": "function", "function": {"name": "noargs", "parameters": {"type": "object", "properties": {}}}}])
    );
    assert_eq!(chat["tool_choice"], json!({"type": "function", "function": {"name": "noargs"}}));
    assert!(chat.get("parallel_tool_calls").is_none());
    assert!(chat.get("stream").is_none());

    let tools = json!([{"type": "local_shell"}]);
    for (choice, expected) in [
        (json!("none"), json!("none")),
        (json!("required"), json!("required")),
        (json!({"type": "local_shell"}), json!({"type": "function", "function": {"name": "local_shell"}})),
        (json!("sometimes"), Value::Null),
        (json!({"type": "web_search"}), Value::Null),
        (json!({"type": "function"}), Value::Null),
    ] {
        let chat = request_to_chat(&json!({"input": [], "tools": tools, "tool_choice": choice})).unwrap();
        assert_eq!(chat["tool_choice"], expected);
        assert_eq!(chat["messages"], json!([]));
    }

    // Only built-in tools Chat cannot express: no tools at all.
    let chat = request_to_chat(&json!({"input": "x", "tools": [{"type": "web_search"}], "tool_choice": "auto"})).unwrap();
    assert!(chat.get("tools").is_none());
    assert!(chat.get("tool_choice").is_none());
}

#[test]
fn completion_to_response_maps_tools_and_usage() {
    let chat = json!({
        "id": "chatcmpl-123",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": "qwen3-coder",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Running it.",
                "reasoning_content": "hidden",
                "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "shell", "arguments": "{\"command\":[\"ls\"]}"}},
                    {"id": "call_2", "type": "function",
                     "function": {"name": "local_shell", "arguments": "{\"action\":{\"type\":\"exec\",\"command\":[\"pwd\"]}}"}},
                    {"id": "call_3", "type": "function",
                     "function": {"name": "local_shell", "arguments": "{not json"}}
                ]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "total_tokens": 1050,
            "prompt_tokens_details": {"cached_tokens": 800},
            "completion_tokens_details": {"reasoning_tokens": 20}
        }
    });
    assert_eq!(
        completion_to_response(&chat),
        json!({
            "id": "resp_chatcmpl-123",
            "object": "response",
            "created_at": 1_700_000_000,
            "status": "completed",
            "model": "qwen3-coder",
            "output": [
                {"type": "message", "id": "msg_chatcmpl-123", "status": "completed", "role": "assistant",
                 "content": [{"type": "output_text", "text": "Running it.", "annotations": []}]},
                {"type": "function_call", "id": "fc_call_1", "call_id": "call_1", "name": "shell",
                 "arguments": "{\"command\":[\"ls\"]}", "status": "completed"},
                {"type": "local_shell_call", "id": "lsh_call_2", "call_id": "call_2",
                 "action": {"type": "exec", "command": ["pwd"]}, "status": "completed"},
                {"type": "local_shell_call", "id": "lsh_call_3", "call_id": "call_3", "action": {}, "status": "completed"}
            ],
            "usage": {
                "input_tokens": 1000,
                "input_tokens_details": {"cached_tokens": 800},
                "output_tokens": 50,
                "output_tokens_details": {"reasoning_tokens": 20},
                "total_tokens": 1050
            }
        })
    );

    let truncated = completion_to_response(&json!({
        "id": "x", "model": "m",
        "choices": [{"message": {"content": "partial", "tool_calls": null}, "finish_reason": "length"}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "prompt_tokens_details": null}
    }));
    assert_eq!(truncated["status"], "incomplete");
    assert_eq!(truncated["incomplete_details"], json!({"reason": "max_output_tokens"}));
    assert_eq!(truncated["created_at"], 0);
    assert_eq!(truncated["output"][0]["content"][0]["text"], "partial");
    assert_eq!(
        truncated["usage"],
        json!({
            "input_tokens": 7, "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 3, "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 10
        })
    );

    let filtered = completion_to_response(&json!({"choices": [{"message": {"content": ""}, "finish_reason": "content_filter"}]}));
    assert_eq!(filtered["status"], "incomplete");
    assert_eq!(filtered["incomplete_details"], json!({"reason": "content_filter"}));
    assert_eq!(filtered["output"], json!([]));
    assert_eq!(filtered["usage"], Value::Null);

    for finish in [json!("stop"), json!("tool_calls"), Value::Null] {
        let r = completion_to_response(&json!({"choices": [{"message": {}, "finish_reason": finish}]}));
        assert_eq!(r["status"], "completed");
        assert!(r.get("incomplete_details").is_none());
    }
    // Garbage in, well-formed response out.
    let garbage = completion_to_response(&json!(42));
    assert_eq!(garbage["id"], "resp_");
    assert_eq!(garbage["object"], "response");
    assert_eq!(garbage["output"], json!([]));
}

fn chunk(delta: Value, finish: Value) -> Value {
    json!({
        "id": "chatcmpl-9",
        "object": "chat.completion.chunk",
        "created": 1_700_000_001,
        "model": "upstream-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
    })
}

#[test]
fn stream_text_then_function_call_then_local_shell() {
    let mut t = StreamTranslator::new("gpt-5-codex");
    let mut blobs = Vec::new();
    let chunks = [
        chunk(json!({"role": "assistant", "content": ""}), Value::Null),
        chunk(json!({"reasoning_content": "thinking..."}), Value::Null),
        chunk(json!({"content": "Let me "}), Value::Null),
        chunk(json!({"content": "check."}), Value::Null),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
                                   "function": {"name": "shell", "arguments": ""}}]}),
            Value::Null,
        ),
        chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"command\":"}}]}), Value::Null),
        chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "[\"ls\"]}"}}]}), Value::Null),
        chunk(
            json!({"tool_calls": [{"index": 1, "id": "call_b", "type": "function",
                                   "function": {"name": "local_shell", "arguments": "{\"action\":{\"type\":\"exec\","}}]}),
            Value::Null,
        ),
        chunk(json!({"tool_calls": [{"index": 1, "function": {"arguments": "\"command\":[\"pwd\"]}}"}}]}), Value::Null),
        chunk(json!({}), json!("tool_calls")),
        json!({
            "id": "chatcmpl-9", "object": "chat.completion.chunk", "choices": [],
            "usage": {"prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150,
                      "prompt_tokens_details": {"cached_tokens": 100},
                      "completion_tokens_details": {"reasoning_tokens": 12}}
        }),
    ];
    for c in &chunks {
        blobs.extend(t.chunk(c));
    }
    blobs.extend(t.finish());
    assert!(t.finish().is_empty(), "finish emits exactly once");
    assert!(t.chunk(&chunks[2]).is_empty(), "nothing after response.completed");

    let evs = events(&blobs);
    assert_eq!(
        types(&evs),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ]
    );

    let in_progress = json!({
        "id": "resp_chatcmpl-9", "object": "response", "created_at": 1_700_000_001,
        "status": "in_progress", "model": "gpt-5-codex", "output": [], "usage": null
    });
    assert_eq!(evs[0].1, json!({"type": "response.created", "sequence_number": 0, "response": in_progress}));
    assert_eq!(evs[1].1, json!({"type": "response.in_progress", "sequence_number": 1, "response": in_progress}));

    // Text item.
    assert_eq!(
        evs[2].1,
        json!({"type": "response.output_item.added", "sequence_number": 2, "output_index": 0,
               "item": {"type": "message", "id": "msg_chatcmpl-9", "status": "in_progress", "role": "assistant", "content": []}})
    );
    assert_eq!(
        evs[3].1,
        json!({"type": "response.content_part.added", "sequence_number": 3, "item_id": "msg_chatcmpl-9",
               "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}})
    );
    assert_eq!(
        evs[4].1,
        json!({"type": "response.output_text.delta", "sequence_number": 4, "item_id": "msg_chatcmpl-9",
               "output_index": 0, "content_index": 0, "delta": "Let me "})
    );
    assert_eq!(evs[5].1["delta"], "check.");
    assert_eq!(
        evs[6].1,
        json!({"type": "response.output_text.done", "sequence_number": 6, "item_id": "msg_chatcmpl-9",
               "output_index": 0, "content_index": 0, "text": "Let me check."})
    );
    assert_eq!(
        evs[7].1,
        json!({"type": "response.content_part.done", "sequence_number": 7, "item_id": "msg_chatcmpl-9",
               "output_index": 0, "content_index": 0,
               "part": {"type": "output_text", "text": "Let me check.", "annotations": []}})
    );
    let message = json!({
        "type": "message", "id": "msg_chatcmpl-9", "status": "completed", "role": "assistant",
        "content": [{"type": "output_text", "text": "Let me check.", "annotations": []}]
    });
    assert_eq!(evs[8].1, json!({"type": "response.output_item.done", "sequence_number": 8, "output_index": 0, "item": message}));

    // Function call item.
    assert_eq!(
        evs[9].1,
        json!({"type": "response.output_item.added", "sequence_number": 9, "output_index": 1,
               "item": {"type": "function_call", "id": "fc_call_a", "call_id": "call_a", "name": "shell",
                        "arguments": "", "status": "in_progress"}})
    );
    assert_eq!(
        evs[10].1,
        json!({"type": "response.function_call_arguments.delta", "sequence_number": 10, "item_id": "fc_call_a",
               "output_index": 1, "delta": "{\"command\":"})
    );
    assert_eq!(evs[11].1["delta"], "[\"ls\"]}");
    assert_eq!(
        evs[12].1,
        json!({"type": "response.function_call_arguments.done", "sequence_number": 12, "item_id": "fc_call_a",
               "output_index": 1, "arguments": "{\"command\":[\"ls\"]}"})
    );
    let function_call = json!({
        "type": "function_call", "id": "fc_call_a", "call_id": "call_a", "name": "shell",
        "arguments": "{\"command\":[\"ls\"]}", "status": "completed"
    });
    assert_eq!(evs[13].1, json!({"type": "response.output_item.done", "sequence_number": 13, "output_index": 1, "item": function_call}));

    // Local shell call item: its own shape in added and done.
    assert_eq!(
        evs[14].1,
        json!({"type": "response.output_item.added", "sequence_number": 14, "output_index": 2,
               "item": {"type": "local_shell_call", "id": "lsh_call_b", "call_id": "call_b", "action": {}, "status": "in_progress"}})
    );
    assert_eq!(evs[15].1["item_id"], "lsh_call_b");
    assert_eq!(evs[15].1["output_index"], 2);
    assert_eq!(evs[16].1["delta"], "\"command\":[\"pwd\"]}}");
    assert_eq!(evs[17].1["arguments"], "{\"action\":{\"type\":\"exec\",\"command\":[\"pwd\"]}}");
    let shell_call = json!({
        "type": "local_shell_call", "id": "lsh_call_b", "call_id": "call_b",
        "action": {"type": "exec", "command": ["pwd"]}, "status": "completed"
    });
    assert_eq!(evs[18].1, json!({"type": "response.output_item.done", "sequence_number": 18, "output_index": 2, "item": shell_call}));

    assert_eq!(
        evs[19].1,
        json!({
            "type": "response.completed",
            "sequence_number": 19,
            "response": {
                "id": "resp_chatcmpl-9", "object": "response", "created_at": 1_700_000_001,
                "status": "completed", "model": "gpt-5-codex",
                "output": [message, function_call, shell_call],
                "usage": {
                    "input_tokens": 120, "input_tokens_details": {"cached_tokens": 100},
                    "output_tokens": 30, "output_tokens_details": {"reasoning_tokens": 12},
                    "total_tokens": 150
                }
            }
        })
    );

    // Reassembled arguments equal the done payload.
    let args: String = evs[10..12].iter().map(|(_, d)| d["delta"].as_str().unwrap()).collect();
    assert_eq!(args, evs[12].1["arguments"]);
}

#[test]
fn stream_edge_cases() {
    // No chunks at all: still a well-formed response.
    let mut t = StreamTranslator::new("m");
    let evs = events(&t.finish());
    assert_eq!(types(&evs), ["response.created", "response.in_progress", "response.completed"]);
    let response = &evs[2].1["response"];
    assert_eq!(response["status"], "completed");
    assert_eq!(response["output"], json!([]));
    assert_eq!(response["usage"], Value::Null);
    assert_eq!(response["model"], "m");
    assert!(response["id"].as_str().unwrap().starts_with("resp_"));
    assert_eq!(evs[0].1["response"]["id"], response["id"]);
    assert!(t.finish().is_empty());

    // Truncated stream: finish closes the open item; text after a tool call
    // opens a new message item; ids repeated on every fragment do not start
    // new calls; a call without arguments completes with `{}`.
    let mut t = StreamTranslator::new("m");
    let mut blobs = Vec::new();
    blobs.extend(
        t.chunk(&chunk(json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "A", "arguments": "{"}}]}), Value::Null)),
    );
    blobs.extend(t.chunk(&chunk(json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"arguments": "}"}}]}), Value::Null)));
    blobs.extend(t.chunk(&chunk(json!({"tool_calls": [{"index": 1, "id": "c2", "function": {"name": "B"}}]}), Value::Null)));
    blobs.extend(t.chunk(&chunk(json!({"content": "done"}), Value::Null)));
    blobs.extend(t.chunk(&json!({"garbage": true})));
    blobs.extend(t.chunk(&json!(null)));
    blobs.extend(t.finish());
    let evs = events(&blobs);
    assert_eq!(
        types(&evs),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(evs[2].1["item"]["call_id"], "c1");
    assert_eq!(evs[5].1["arguments"], "{}");
    assert_eq!(evs[8].1["arguments"], "{}");
    assert_eq!(evs[10].1["output_index"], 2);
    assert_eq!(evs[10].1["item"]["id"], "msg_chatcmpl-9_2");
    let output = &evs[16].1["response"]["output"];
    assert_eq!(output.as_array().unwrap().len(), 3);
    assert_eq!(output[0]["arguments"], "{}");
    assert_eq!(output[1]["name"], "B");
    assert_eq!(output[2]["content"][0]["text"], "done");
    assert_eq!(evs[16].1["response"]["status"], "completed");

    // Backends that reuse index 0 with a fresh id per call; finish_reason
    // "length" makes the response incomplete.
    let mut t = StreamTranslator::new("m");
    let mut blobs = Vec::new();
    for id in ["x1", "x2"] {
        blobs.extend(
            t.chunk(&chunk(json!({"tool_calls": [{"index": 0, "id": id, "function": {"name": "T", "arguments": "{}"}}]}), Value::Null)),
        );
    }
    blobs.extend(t.chunk(&chunk(json!({}), json!("length"))));
    blobs.extend(t.finish());
    let evs = events(&blobs);
    let added: Vec<(u64, &str)> = evs
        .iter()
        .filter(|(e, _)| e == "response.output_item.added")
        .map(|(_, d)| (d["output_index"].as_u64().unwrap(), d["item"]["call_id"].as_str().unwrap()))
        .collect();
    assert_eq!(added, [(0, "x1"), (1, "x2")]);
    let (last, data) = evs.last().unwrap();
    assert_eq!(last, "response.completed");
    assert_eq!(data["response"]["status"], "incomplete");
    assert_eq!(data["response"]["incomplete_details"], json!({"reason": "max_output_tokens"}));
    assert_eq!(data["response"]["output"].as_array().unwrap().len(), 2);
}
