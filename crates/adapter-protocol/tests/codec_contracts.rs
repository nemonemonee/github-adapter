use std::fmt::Debug;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use adapter_protocol::anthropic;
use adapter_protocol::chat::{chat_to_response, response_events, responses_to_chat};
use adapter_protocol::chat_stream::ChatResponseStream;
use adapter_protocol::error::Result;
use adapter_protocol::sse::SseEvent;
use serde_json::{Value, json};

fn parse(raw: &str) -> Value {
    serde_json::from_str(raw).expect("valid test JSON")
}

fn assert_error<T: Debug>(result: Result<T>, status: u16) {
    let error = result.expect_err("invalid input must fail");
    assert_eq!(error.status, status, "{error}");
    assert!(!error.message.is_empty());
}

fn function_tool(name: &str) -> Value {
    json!({
        "type": "function", "name": name, "description": "Look up a value",
        "parameters": {
            "type": "object", "properties": {"key": {"type": "string"}},
            "required": ["key"], "additionalProperties": false,
        },
        "strict": true,
    })
}

fn function_call(id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "type": "function_call", "id": format!("fc_{id}"), "status": "completed",
        "call_id": id, "name": name, "arguments": arguments,
    })
}

fn function_output(id: &str, output: Value) -> Value {
    json!({"type": "function_call_output", "call_id": id, "output": output})
}

fn chat_call(id: &str, name: &str, arguments: &str) -> Value {
    json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments}})
}

fn completion(content: Value, finish: &str) -> Value {
    json!({
        "id": "chatcmpl_example", "object": "chat.completion", "created": 1_780_000_000,
        "model": "provider-model",
        "choices": [{"index": 0, "finish_reason": finish, "message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14},
    })
}

fn convert(source: &Value) -> Value {
    chat_to_response(source, &json!({"model": "requested-model"})).expect("valid completion")
}

fn data(event: &SseEvent) -> &Value {
    assert!(!event.done);
    assert!(event.comment.is_none());
    event.data.as_ref().expect("JSON event")
}

fn names(events: &[SseEvent]) -> Vec<&str> {
    events.iter().map(|event| event.kind.as_str()).collect()
}

fn assert_sequences(events: &[SseEvent]) {
    for (index, event) in events.iter().enumerate() {
        assert_eq!(data(event)["sequence_number"], json!(index));
        assert_eq!(data(event)["type"], event.kind);
    }
}

fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id": "chat_fixture", "object": "chat.completion.chunk", "created": 1,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
}

fn stream() -> ChatResponseStream {
    ChatResponseStream::new(
        &json!({"model": "fixture", "input": "hello"}),
        Some("resp_fixture"),
    )
    .unwrap()
}

fn anthropic_request() -> Value {
    json!({"messages": [{"role": "user", "content": "hello"}]})
}

fn native_response() -> Value {
    json!({
        "id": "resp_fixture", "status": "completed",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "Hello"}]}],
        "usage": {"input_tokens": 10, "output_tokens": 4},
    })
}

#[test]
fn nested_tool_json_preserves_reserved_keys_and_uses_shared_structural_limits() {
    for (arguments, expected) in [
        (
            r#"{"$serde_json::private::Number":"NaN"}"#,
            json!({"$serde_json::private::Number": "NaN"}),
        ),
        (
            r#"{"$serde_json::private::RawValue":"[1,2]","nested":{"$serde_json::private::Number":"123"}}"#,
            json!({
                "$serde_json::private::RawValue": "[1,2]",
                "nested": {"$serde_json::private::Number": "123"},
            }),
        ),
    ] {
        let mut response = native_response();
        response["output"] = json!([{
            "type": "function_call", "call_id": "a", "name": "lookup", "arguments": arguments,
        }]);
        let original = response.clone();
        let message = anthropic::to_message(&response, "requested").unwrap();
        assert_eq!(message["content"][0]["input"], expected);
        assert_eq!(response, original);
    }

    let depth = adapter_protocol::json::MAX_JSON_DEPTH;
    let number_bytes = adapter_protocol::json::MAX_NUMBER_BYTES;
    for (arguments, accepted) in [
        (
            format!(
                "{{\"value\":{}0{}}}",
                "[".repeat(depth - 1),
                "]".repeat(depth - 1)
            ),
            true,
        ),
        (
            format!("{{\"value\":{}0{}}}", "[".repeat(depth), "]".repeat(depth)),
            false,
        ),
        (
            format!("{{\"value\":1{}}}", "0".repeat(number_bytes - 1)),
            true,
        ),
        (
            format!("{{\"value\":1{}}}", "0".repeat(number_bytes)),
            false,
        ),
    ] {
        let mut response = native_response();
        response["output"] = json!([{
            "type": "function_call", "call_id": "a", "name": "lookup", "arguments": arguments,
        }]);
        let result = anthropic::to_message(&response, "requested");
        if accepted {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert_error(result, 502);
        }
    }
}

#[test]
fn integer_negative_zero_matches_python_without_accepting_float_or_boolean_zero() {
    let zero = adapter_protocol::json::decode(b"-0", 2).unwrap();
    let baseline = responses_to_chat(&json!({"model": "m", "input": "x"}), false).unwrap();
    assert_eq!(
        responses_to_chat(
            &json!({"model": "m", "input": "x", "top_logprobs": zero}),
            false
        )
        .unwrap(),
        baseline,
    );
    assert_error(
        responses_to_chat(
            &json!({"model": "m", "input": "x", "max_output_tokens": zero}),
            false,
        ),
        400,
    );
    for invalid in [json!(false), json!(0.0), json!(-0.0)] {
        assert_error(
            responses_to_chat(
                &json!({"model": "m", "input": "x", "top_logprobs": invalid}),
                false,
            ),
            400,
        );
    }
    let mut source = completion(json!("Hello"), "stop");
    source["created"] = zero.clone();
    source["choices"][0]["index"] = zero.clone();
    assert_eq!(convert(&source)["created_at"], json!(0));

    let mut codec = stream();
    codec.start().unwrap();
    let mut terminal = chunk(json!({"content": "Hello"}), Some("stop"));
    terminal["choices"][0]["index"] = zero;
    codec.feed(&terminal).unwrap();
    codec.finish().unwrap();
    assert_eq!(codec.response().unwrap()["output_text"], "Hello");
}

#[test]
fn replay_assigned_language_neutral_fixtures() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("conformance")
        .join("fixtures");
    let mut paths: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    let mut replayed = 0;
    for path in paths {
        let fixture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let input = &fixture["input"];
        let result = match fixture["operation"].as_str() {
            Some("responses_to_chat") => responses_to_chat(
                &input["request"],
                input["stream"].as_bool().unwrap_or(false),
            ),
            Some("anthropic_request") => anthropic::to_responses(
                &input["request"],
                input["model"].as_str().unwrap(),
                input["force_effort"].as_str(),
                input["max_output_tokens_cap"].as_u64(),
            ),
            _ => continue,
        };
        assert_eq!(fixture["version"], 1, "{}", path.display());
        if fixture.get("expected_error").is_some() {
            let error = result.expect_err("fixture requires an error");
            assert_eq!(
                json!(error.status),
                fixture["expected_error"]["status"],
                "{}",
                path.display()
            );
        } else {
            assert_eq!(result.unwrap(), fixture["expected"], "{}", path.display());
        }
        replayed += 1;
    }
    assert!(replayed >= 3, "the assigned fixture set must not disappear");
}

#[test]
fn chat_request_maps_generation_controls_and_explicit_upstream_streaming() {
    let request = json!({
        "model": "requested-model", "instructions": "Be precise.", "input": "Hello",
        "stream": true, "max_output_tokens": 128, "temperature": 0, "top_p": 0.8,
        "parallel_tool_calls": false,
    });
    let original = request.clone();
    assert_eq!(
        responses_to_chat(&request, false).unwrap(),
        json!({
            "model": "requested-model", "stream": false, "max_completion_tokens": 128,
            "temperature": 0, "top_p": 0.8, "parallel_tool_calls": false,
            "messages": [
                {"role": "system", "content": "Be precise."},
                {"role": "user", "content": "Hello"},
            ],
        })
    );
    let streamed = responses_to_chat(&request, true).unwrap();
    assert_eq!(streamed["stream"], true);
    assert_eq!(streamed["stream_options"], json!({"include_usage": true}));
    assert_eq!(request, original);
}

#[test]
fn multimodal_parallel_history_preserves_order_ids_and_argument_bytes() {
    let arguments = "{ \"key\": \"a\\\\b\", \"note\": \"\\u2603\" }\n";
    let request = json!({
        "model": "m", "instructions": "System.",
        "input": [
            {"role": "developer", "content": "Use tools."},
            {"role": "user", "content": [
                {"type": "input_text", "text": "Compare "},
                {"type": "input_image", "image_url": "https://example.invalid/a.png", "detail": "high"},
                {"type": "input_text", "text": " with "},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "low"},
            ]},
            {"type": "message", "id": "msg_old", "status": "completed", "role": "assistant", "content": [
                {"type": "output_text", "text": "Looking.", "annotations": [], "logprobs": []},
            ]},
            function_call("call_a", "lookup", arguments),
            function_call("call_b", "other", "{\"key\":\"b\"}"),
            function_output("call_b", json!("second first")),
            function_output("call_a", json!("first second")),
            {"role": "assistant", "content": "Done."},
            {"role": "user", "content": "Continue."},
        ],
    });
    let original = request.clone();
    let result = responses_to_chat(&request, false).unwrap();
    let messages = result["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "system",
            "developer",
            "user",
            "assistant",
            "tool",
            "tool",
            "assistant",
            "user"
        ]
    );
    assert_eq!(
        messages[2]["content"],
        json!([
            {"type": "text", "text": "Compare "},
            {"type": "image_url", "image_url": {"url": "https://example.invalid/a.png", "detail": "high"}},
            {"type": "text", "text": " with "},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA", "detail": "low"}},
        ])
    );
    assert_eq!(
        messages[3]["content"],
        json!([{"type": "text", "text": "Looking."}])
    );
    assert_eq!(
        messages[3]["tool_calls"],
        json!([
            chat_call("call_a", "lookup", arguments),
            chat_call("call_b", "other", "{\"key\":\"b\"}"),
        ])
    );
    assert_eq!(
        messages[4],
        json!({"role": "tool", "tool_call_id": "call_b", "content": "second first"})
    );
    assert_eq!(
        messages[5],
        json!({"role": "tool", "tool_call_id": "call_a", "content": "first second"})
    );
    assert_eq!(request, original);
}

#[test]
fn sequential_tools_and_refusal_history_remain_separate_turns() {
    let result = responses_to_chat(&json!({"model": "m", "input": [
        {"role": "assistant", "content": [{"type": "refusal", "refusal": "Cannot do that."}]},
        {"role": "user", "content": "Use a safe tool."},
        function_call("a", "lookup", "{}"),
        function_output("a", json!([{"type": "input_text", "text": "one"}, {"type": "output_text", "text": "two"}])),
        function_call("b", "lookup", "{}"),
        function_output("b", json!("result")),
    ]}), false).unwrap();
    let messages = &result["messages"];
    assert_eq!(
        messages[0]["content"],
        json!([{"type": "refusal", "refusal": "Cannot do that."}])
    );
    assert!(messages[2]["content"].is_null());
    assert_eq!(
        messages[3]["content"],
        json!([{"type": "text", "text": "one"}, {"type": "text", "text": "two"}])
    );
    assert_eq!(messages[4]["tool_calls"][0]["id"], "b");
    assert_eq!(messages.as_array().unwrap().len(), 6);
}

#[test]
fn empty_text_and_instructions_only_are_valid_inputs() {
    assert_eq!(
        responses_to_chat(&json!({"model": "m", "input": ""}), false).unwrap()["messages"],
        json!([{"role": "user", "content": ""}])
    );
    assert_eq!(
        responses_to_chat(&json!({"model": "m", "instructions": "Begin."}), false).unwrap()["messages"],
        json!([{"role": "system", "content": "Begin."}])
    );
}

#[test]
fn function_tools_choices_and_nullable_schema_fields_are_preserved() {
    for choice in [
        json!("auto"),
        json!("none"),
        json!("required"),
        json!({"type": "function", "name": "lookup"}),
    ] {
        let result = responses_to_chat(&json!({
            "model": "m", "input": "x", "tools": [function_tool("lookup")], "tool_choice": choice,
        }), false).unwrap();
        let expected = if choice.is_object() {
            json!({"type": "function", "function": {"name": "lookup"}})
        } else {
            choice
        };
        assert_eq!(result["tool_choice"], expected);
        assert_eq!(
            result["tools"][0]["function"]["parameters"]["required"],
            json!(["key"])
        );
        assert_eq!(result["tools"][0]["function"]["strict"], true);
    }
    for (tool, expected) in [
        (
            json!({"type": "function", "name": "ping"}),
            json!({"name": "ping"}),
        ),
        (
            json!({"type": "function", "name": "ping", "parameters": null, "description": null, "strict": null}),
            json!({"name": "ping"}),
        ),
        (
            json!({"type": "function", "name": "ping", "parameters": {}, "description": "", "strict": false}),
            json!({"name": "ping", "parameters": {}, "description": "", "strict": false}),
        ),
    ] {
        assert_eq!(
            responses_to_chat(&json!({"model": "m", "input": "x", "tools": [tool]}), false)
                .unwrap()["tools"][0]["function"],
            expected
        );
    }
}

#[test]
fn structured_output_keeps_constraints_and_does_not_alias_input() {
    for strict in [None, Some(false), Some(true)] {
        let mut format = json!({
            "type": "json_schema", "name": "answer", "description": "Constrained answer",
            "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"], "additionalProperties": false},
        });
        if let Some(strict) = strict {
            format["strict"] = json!(strict);
        }
        let request =
            json!({"model": "m", "input": "x", "text": {"format": format, "verbosity": "low"}});
        let original = request.clone();
        let mut result = responses_to_chat(&request, false).unwrap();
        let mut schema = format;
        schema.as_object_mut().unwrap().remove("type");
        assert_eq!(
            result["response_format"],
            json!({"type": "json_schema", "json_schema": schema})
        );
        assert_eq!(result["verbosity"], "low");
        result["response_format"]["json_schema"]["schema"]["properties"]["ok"]["type"] =
            json!("string");
        assert_eq!(request, original);
    }
}

#[test]
fn bookkeeping_and_neutral_options_do_not_become_upstream_features() {
    let baseline = responses_to_chat(&json!({"model": "m", "input": "x"}), false).unwrap();
    let neutral = json!({
        "model": "m", "input": "x", "store": false, "background": false,
        "previous_response_id": "", "conversation": null, "prompt": null, "max_tool_calls": null,
        "truncation": "disabled", "service_tier": "default", "stream_options": {},
        "context_management": [], "top_logprobs": 0, "include": ["reasoning.encrypted_content"],
        "reasoning": {"effort": null, "summary": null, "generate_summary": null},
        "text": {"format": null, "verbosity": null}, "metadata": {"client": "fixture"},
        "prompt_cache_key": "session",
    });
    assert_eq!(responses_to_chat(&neutral, false).unwrap(), baseline);
    for key in [
        "store",
        "background",
        "previous_response_id",
        "conversation",
        "prompt",
        "max_tool_calls",
        "truncation",
        "service_tier",
        "stream_options",
        "context_management",
        "top_logprobs",
        "include",
        "reasoning",
        "text",
        "instructions",
        "stream",
        "metadata",
        "prompt_cache_key",
        "max_output_tokens",
        "temperature",
        "top_p",
        "parallel_tool_calls",
        "tools",
        "tool_choice",
    ] {
        let mut request = json!({"model": "m", "input": "x"});
        request[key] = Value::Null;
        assert_eq!(
            responses_to_chat(&request, false).unwrap(),
            baseline,
            "{key}"
        );
    }
    for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
        let result = responses_to_chat(
            &json!({
                "model": "m", "input": "x", "reasoning": {"effort": effort},
                "text": {"format": {"type": "json_object"}},
            }),
            false,
        )
        .unwrap();
        assert_eq!(result["reasoning_effort"], effort);
        assert_eq!(result["response_format"], json!({"type": "json_object"}));
    }
    let plain = responses_to_chat(
        &json!({
            "model": "m", "input": "x", "text": {"format": {"type": "text"}},
        }),
        false,
    )
    .unwrap();
    assert!(plain.get("response_format").is_none());
}

#[test]
fn malformed_requests_and_non_neutral_controls_fail_preflight() {
    for request in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"model": ""}),
        json!({"model": 1, "input": "x"}),
        json!({"model": "m"}),
        json!({"model": "m", "input": []}),
        json!({"model": "m", "input": null}),
        json!({"model": "m", "input": {}}),
        json!({"model": "m", "input": [1]}),
        json!({"model": "m", "input": ["x"]}),
    ] {
        assert_error(responses_to_chat(&request, false), 400);
    }
    for (key, values) in [
        (
            "max_output_tokens",
            vec![
                json!(0),
                json!(-1),
                json!(1.5),
                json!(true),
                json!("12"),
                parse("18446744073709551616"),
            ],
        ),
        (
            "temperature",
            vec![
                json!(-0.1),
                json!(2.1),
                json!(true),
                json!("1"),
                parse("1e999"),
            ],
        ),
        (
            "top_p",
            vec![json!(-1), json!(1.1), json!(false), json!([])],
        ),
        (
            "parallel_tool_calls",
            vec![json!(0), json!("true"), json!([])],
        ),
        ("stream", vec![json!(1), json!("true"), json!({})]),
        ("metadata", vec![json!([]), json!({"n": 1})]),
        ("prompt_cache_key", vec![json!(1), json!([])]),
        (
            "include",
            vec![
                json!("reasoning.encrypted_content"),
                json!([1]),
                json!(["message.output_text.logprobs"]),
            ],
        ),
        (
            "reasoning",
            vec![
                json!(false),
                json!({"effort": "extreme"}),
                json!({"summary": "auto"}),
                json!({"generate_summary": "concise"}),
                json!({"budget": 1}),
            ],
        ),
        (
            "text",
            vec![
                json!(false),
                json!({"verbosity": "extreme"}),
                json!({"unknown": null}),
            ],
        ),
        ("previous_response_id", vec![json!("resp_old"), json!(0)]),
        (
            "conversation",
            vec![json!("conv_old"), json!({"id": "conv_old"})],
        ),
        ("background", vec![json!(true), json!(0)]),
        ("store", vec![json!(true), json!(0)]),
        ("prompt", vec![json!({"id": "pmpt_old"})]),
        ("max_tool_calls", vec![json!(1)]),
        ("truncation", vec![json!("auto")]),
        ("service_tier", vec![json!("priority")]),
        ("context_management", vec![json!([{"type": "compaction"}])]),
        (
            "stream_options",
            vec![json!({"include_obfuscation": false})],
        ),
        ("top_logprobs", vec![json!(1), json!(false), json!(0.0)]),
        ("instructions", vec![json!([])]),
        ("unknown", vec![Value::Null]),
    ] {
        for value in values {
            let mut request = json!({"model": "m", "input": "x"});
            request[key] = value;
            assert_error(responses_to_chat(&request, false), 400);
        }
    }
}

#[test]
fn chat_never_drops_reasoning_encrypted_items_or_unsupported_attachments() {
    for item in [
        json!({"type": "reasoning", "id": "rs_1", "summary": []}),
        json!({"type": "reasoning", "encrypted_content": "opaque", "summary": []}),
        json!({"type": "item_reference", "id": "msg_old"}),
        json!({"type": "compaction", "encrypted_content": "opaque"}),
        json!({"type": "web_search_call", "id": "ws_1"}),
        json!({"role": "assistant", "content": "x", "encrypted_content": "opaque"}),
        json!({"role": "assistant", "content": "x", "phase": "commentary"}),
    ] {
        assert_error(
            responses_to_chat(
                &json!({
                    "model": "m", "input": [item], "include": ["reasoning.encrypted_content"],
                }),
                false,
            ),
            400,
        );
    }
    for content in [
        Value::Null,
        json!({}),
        json!([null]),
        json!([{"type": "unknown"}]),
        json!([{"type": "input_audio", "data": "AAAA"}]),
        json!([{"type": "input_file", "file_id": "file_1"}]),
        json!([{"type": "input_text"}]),
        json!([{"type": "input_text", "text": 1}]),
        json!([{"type": "output_text", "text": "x", "annotations": [{"type": "citation"}]}]),
        json!([{"type": "output_text", "text": "x", "logprobs": [{"token": "x"}]}]),
        json!([{"type": "input_text", "text": "x", "extra": null}]),
        json!([{"type": "input_image", "file_id": "file_1"}]),
        json!([{"type": "input_image", "image_url": "url", "file_id": "file_1"}]),
        json!([{"type": "input_image", "image_url": ""}]),
        json!([{"type": "input_image", "image_url": {"url": "url"}}]),
        json!([{"type": "input_image", "image_url": "url", "detail": "invalid"}]),
        json!([{"type": "refusal", "refusal": "No."}]),
    ] {
        assert_error(
            responses_to_chat(
                &json!({"model": "m", "input": [{"role": "user", "content": content}]}),
                false,
            ),
            400,
        );
    }
    for item in [
        json!({"role": "tool", "content": "x"}),
        json!({"role": "unknown", "content": "x"}),
        json!({"content": "x"}),
        json!({"role": "user", "content": "x", "status": "failed"}),
        json!({"role": "user", "content": "x", "id": 1}),
        json!({"role": "user", "type": null, "content": "x"}),
        json!({"role": "assistant", "content": [{"type": "input_image", "image_url": "url"}]}),
    ] {
        assert_error(
            responses_to_chat(&json!({"model": "m", "input": [item]}), false),
            400,
        );
    }
}

#[test]
fn incomplete_duplicate_and_misordered_tool_history_is_rejected() {
    let call = function_call("a", "lookup", "{}");
    let output = function_output("a", json!("result"));
    for history in [
        json!([output]),
        json!([call]),
        json!([call, {"role": "user", "content": "x"}]),
        json!([call, function_output("other", json!("x"))]),
        json!([call, call, output]),
        json!([call, output, output]),
        json!([
            call,
            function_call("b", "lookup", "{}"),
            output,
            function_call("c", "lookup", "{}")
        ]),
        json!([call, function_output("a", json!({}))]),
        json!([
            call,
            function_output("a", json!([{"type": "input_image", "image_url": "url"}]))
        ]),
    ] {
        assert_error(
            responses_to_chat(&json!({"model": "m", "input": history}), false),
            400,
        );
    }
    for (key, value) in [
        ("name", json!("")),
        ("call_id", json!("")),
        ("arguments", json!({})),
        ("arguments", Value::Null),
    ] {
        let mut invalid = call.clone();
        invalid[key] = value;
        assert_error(
            responses_to_chat(&json!({"model": "m", "input": [invalid, output]}), false),
            400,
        );
    }
}

#[test]
fn malformed_tools_choices_and_structured_formats_are_rejected() {
    for tools in [
        json!("function"),
        json!(0),
        json!(false),
        json!({}),
        json!([null]),
        json!([{"type": "web_search"}]),
        json!([{"type": "function"}]),
        json!([{"type": "function", "name": ""}]),
        json!([{"type": "function", "function": {"name": "nested"}}]),
        json!([{"type": "function", "name": "x", "parameters": []}]),
        json!([{"type": "function", "name": "x", "strict": 1}]),
        json!([{"type": "function", "name": "x", "description": 1}]),
        json!([function_tool("same"), function_tool("same")]),
        parse(r#"[{"type":"function","name":"x","parameters":{"bad":1e999}}]"#),
    ] {
        assert_error(
            responses_to_chat(&json!({"model": "m", "input": "x", "tools": tools}), false),
            400,
        );
    }
    for choice in [
        json!("unsupported"),
        json!(false),
        json!([]),
        json!({"type": "web_search"}),
        json!({"type": "function", "name": "missing"}),
        json!({"type": "function", "function": {"name": "lookup"}}),
        json!({"type": "function", "name": ""}),
    ] {
        assert_error(
            responses_to_chat(
                &json!({
                    "model": "m", "input": "x", "tools": [function_tool("lookup")], "tool_choice": choice,
                }),
                false,
            ),
            400,
        );
    }
    for choice in [
        json!("required"),
        json!({"type": "function", "name": "lookup"}),
    ] {
        assert_error(
            responses_to_chat(
                &json!({"model": "m", "input": "x", "tool_choice": choice}),
                false,
            ),
            400,
        );
    }
    for format in [
        json!([]),
        json!({}),
        json!({"type": "xml"}),
        json!({"type": "text", "schema": {}}),
        json!({"type": "json_object", "strict": true}),
        json!({"type": "json_schema", "schema": {}}),
        json!({"type": "json_schema", "name": "x"}),
        json!({"type": "json_schema", "name": "x", "schema": []}),
        json!({"type": "json_schema", "name": "x", "schema": {}, "strict": "true"}),
        json!({"type": "json_schema", "name": "x", "schema": {}, "description": 1}),
    ] {
        assert_error(
            responses_to_chat(
                &json!({"model": "m", "input": "x", "text": {"format": format}}),
                false,
            ),
            400,
        );
    }
}

#[test]
fn buffered_response_matches_python_resource_and_hash_snapshot() {
    assert_eq!(
        convert(&completion(json!("Hello"), "stop")),
        json!({
            "id": "resp_b2603f11beb5beb7affdbc8221bad9e4",
            "object": "response", "created_at": 1_780_000_000, "model": "requested-model",
            "status": "completed", "error": null, "incomplete_details": null,
            "output": [{
                "id": "msg_dffd267f0aa12987f8703c199f1f223f", "type": "message",
                "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "Hello", "annotations": []}],
            }],
            "output_text": "Hello", "usage": {"input_tokens": 10, "output_tokens": 4, "total_tokens": 14},
            "store": false, "background": false, "previous_response_id": null,
            "reasoning": {"effort": null, "summary": null}, "text": {"format": {"type": "text"}},
            "instructions": null, "max_output_tokens": null, "parallel_tool_calls": true,
            "temperature": 1, "top_p": 1, "tool_choice": "auto", "tools": [], "metadata": {},
            "truncation": "disabled",
        })
    );
}

#[test]
fn stable_hashes_match_python_ascii_escaping_unicode_sorting_and_float_notation() {
    let mut source = completion(json!("Hello"), "stop");
    source["id"] = json!("upstream_雪\n\"\\");
    let response = chat_to_response(&source, &json!({"model": "模型😀\u{7f}"})).unwrap();
    assert_eq!(response["id"], "resp_830b8f207694cb425846e1c388ef9222");
    let mut source = completion(json!("Hello"), "stop");
    source.as_object_mut().unwrap().remove("id");
    source["extra"] = parse(
        r#"{"z":[1.0,-0.0,1e-5,1e-4,1e15,1e16,1e20,1e-7,5e-324,1.7976931348623157e308],"a":{"é":"雪😀\u0000\u007f","x":123456789012345678901234567890}}"#,
    );
    let result = convert(&source);
    assert_eq!(result["id"], "resp_0a51ea44808e2d343bee74b9a5aa165d");
    let reversed = Value::Object(
        source
            .as_object()
            .unwrap()
            .iter()
            .rev()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    assert_eq!(convert(&reversed), result);
    source["choices"][0]["message"]["content"] = json!("Different");
    assert_ne!(convert(&source)["id"], result["id"]);
}

#[test]
fn missing_envelope_values_use_local_time_and_deterministic_content_identity() {
    let mut source = completion(json!("Hello"), "stop");
    for key in ["id", "object", "created", "usage"] {
        source.as_object_mut().unwrap().remove(key);
    }
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let first = convert(&source);
    let second = convert(&source);
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(first["id"], second["id"]);
    assert_eq!(first["output"][0]["id"], second["output"][0]["id"]);
    assert!((before..=after).contains(&first["created_at"].as_u64().unwrap()));
    assert!(first["usage"].is_null());
    assert_eq!(first["model"], "requested-model");
}

#[test]
fn response_defaults_and_nested_values_are_immutable() {
    let source = completion(json!("Text"), "stop");
    let request = json!({
        "model": "alias", "tools": [function_tool("lookup")], "metadata": {"client": "test"},
        "reasoning": {"effort": "low"},
        "text": {"format": {"type": "json_schema", "name": "x", "schema": {"properties": {"x": {"type": "string"}}}}},
    });
    let original = (source.clone(), request.clone());
    let mut response = chat_to_response(&source, &request).unwrap();
    assert_eq!(
        response["reasoning"],
        json!({"effort": "low", "summary": null})
    );
    response["tools"][0]["parameters"]["properties"]["key"]["type"] = json!("number");
    response["text"]["format"]["schema"]["properties"]["x"]["type"] = json!("number");
    response["metadata"]["client"] = json!("changed");
    response["output"][0]["content"][0]["annotations"]
        .as_array_mut()
        .unwrap()
        .push(json!({"changed": true}));
    assert_eq!((source.clone(), request), original);
    let defaults = chat_to_response(
        &source,
        &json!({
            "model": "m", "tools": null, "tool_choice": null, "metadata": null,
            "parallel_tool_calls": null, "text": {"format": null}, "truncation": null,
        }),
    )
    .unwrap();
    assert_eq!(defaults["tools"], json!([]));
    assert_eq!(defaults["tool_choice"], "auto");
    assert_eq!(defaults["metadata"], json!({}));
    assert_eq!(defaults["parallel_tool_calls"], true);
    assert_eq!(defaults["text"], json!({"format": {"type": "text"}}));
}

#[test]
fn tool_completions_keep_parallel_calls_and_round_trip_as_complete_history() {
    let arguments = "{ \"key\": \"a\\\\b\" }\n";
    for content in [Value::Null, json!(""), json!([]), json!("Checking")] {
        let mut source = completion(content.clone(), "tool_calls");
        source["choices"][0]["message"]["tool_calls"] = json!([
            chat_call("first", "lookup", arguments),
            chat_call("second", "other", ""),
        ]);
        let response = convert(&source);
        let offset = usize::from(content == "Checking");
        assert_eq!(response["output"].as_array().unwrap().len(), offset + 2);
        assert_eq!(response["output"][offset]["call_id"], "first");
        assert_eq!(response["output"][offset]["arguments"], arguments);
        assert_eq!(response["output"][offset + 1]["call_id"], "second");
        assert_eq!(response["output"][offset + 1]["arguments"], "");
        assert_ne!(
            response["output"][offset]["id"],
            response["output"][offset + 1]["id"]
        );
        let mut input = vec![json!({"role": "user", "content": "Look up."})];
        input.extend(response["output"].as_array().unwrap().clone());
        input.extend([
            function_output("second", json!("B")),
            function_output("first", json!("A")),
        ]);
        let history = responses_to_chat(&json!({"model": "m", "input": input}), false).unwrap();
        assert_eq!(history["messages"].as_array().unwrap().len(), 4);
        assert_eq!(
            history["messages"][1]["tool_calls"][0]["function"]["arguments"],
            arguments
        );
        assert_eq!(history["messages"][2]["tool_call_id"], "second");
    }
}

#[test]
fn usage_is_unknown_or_checked_without_double_counting_reasoning() {
    for (usage, expected) in [
        (Value::Null, Value::Null),
        (
            json!({"prompt_tokens": 0, "completion_tokens": 0}),
            json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}),
        ),
        (
            json!({"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14,
                "prompt_tokens_details": {"cached_tokens": 6, "audio_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 2, "accepted_prediction_tokens": 0}}),
            json!({"input_tokens": 10, "output_tokens": 4, "total_tokens": 14,
                "input_tokens_details": {"cached_tokens": 6}, "output_tokens_details": {"reasoning_tokens": 2}}),
        ),
        (
            json!({"prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 173, "reasoning_tokens": 157}),
            json!({"input_tokens": 10, "output_tokens": 163, "total_tokens": 173, "output_tokens_details": {"reasoning_tokens": 157}}),
        ),
        (
            json!({"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14, "reasoning_tokens": 2}),
            json!({"input_tokens": 10, "output_tokens": 4, "total_tokens": 14, "output_tokens_details": {"reasoning_tokens": 2}}),
        ),
        (
            json!({"prompt_tokens": 0, "completion_tokens": 0, "prompt_tokens_details": {"cached_tokens": null}, "completion_tokens_details": null}),
            json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}),
        ),
    ] {
        let mut source = completion(json!("Hello"), "stop");
        source["usage"] = usage;
        let original = source.clone();
        assert_eq!(convert(&source)["usage"], expected);
        assert_eq!(source, original);
    }
}

#[test]
fn malformed_conflicting_and_overflowing_usage_is_an_upstream_error() {
    let mut usages = vec![
        json!([]),
        json!({}),
        json!({"prompt_tokens": 1}),
        json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 3}),
        json!({"prompt_tokens": 10, "completion_tokens": 6, "reasoning_tokens": 2}),
        json!({"prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 30, "reasoning_tokens": 2}),
        json!({"prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 16, "reasoning_tokens": 7}),
        json!({"prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 16,
            "reasoning_tokens": 2, "completion_tokens_details": {"reasoning_tokens": 3}}),
        json!({"prompt_tokens": u64::MAX, "completion_tokens": 1}),
    ];
    for key in [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "reasoning_tokens",
    ] {
        for value in [json!(-1), json!(true), json!(1.0), json!("1")] {
            let mut usage = completion(json!("x"), "stop")["usage"].clone();
            usage[key] = value;
            usages.push(usage);
        }
    }
    for (source, detail, count) in [
        ("prompt_tokens_details", "cached_tokens", json!(11)),
        ("completion_tokens_details", "reasoning_tokens", json!(5)),
        ("prompt_tokens_details", "cached_tokens", json!(-1)),
        ("completion_tokens_details", "reasoning_tokens", json!(true)),
        (
            "completion_tokens_details",
            "rejected_prediction_tokens",
            json!("1"),
        ),
    ] {
        let mut usage = completion(json!("x"), "stop")["usage"].clone();
        let mut details = json!({});
        details[detail] = count;
        usage[source] = details;
        usages.push(usage);
    }
    for usage in usages {
        let mut source = completion(json!("Hello"), "stop");
        source["usage"] = usage;
        assert_error(chat_to_response(&source, &json!({"model": "m"})), 502);
    }
}

#[test]
fn incomplete_empty_or_partial_tool_results_never_become_completed() {
    for finish in ["length", "content_filter"] {
        for content in [Value::Null, json!("Partial")] {
            let mut source = completion(content, finish);
            source["choices"][0]["message"]["tool_calls"] =
                json!([chat_call("a", "lookup", "{\"key\":")]);
            let response = convert(&source);
            assert_eq!(response["status"], "incomplete");
            assert_eq!(
                response["incomplete_details"]["reason"],
                if finish == "length" {
                    "max_output_tokens"
                } else {
                    "content_filter"
                }
            );
            assert!(
                response["output"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["status"] == "incomplete")
            );
            assert_eq!(
                response["output"].as_array().unwrap().last().unwrap()["arguments"],
                "{\"key\":"
            );
            let events = response_events(&response).unwrap();
            assert_eq!(events.last().unwrap().kind, "response.incomplete");
            assert!(!names(&events).contains(&"response.completed"));
        }
        let response = convert(&completion(Value::Null, finish));
        assert_eq!(response["output"], json!([]));
        assert_eq!(response_events(&response).unwrap().len(), 3);
    }
}

#[test]
fn completion_preserves_refusals_content_order_and_legitimate_repetition() {
    let mut source = completion(
        json!([
            {"type": "text", "text": "again"}, {"type": "refusal", "refusal": "No."},
            {"type": "text", "text": "again"}, {"type": "text", "text": ""},
        ]),
        "stop",
    );
    source["choices"][0]["message"]["refusal"] = json!("Cannot comply.");
    let response = convert(&source);
    assert_eq!(response["output_text"], "againagain");
    assert_eq!(
        response["output"][0]["content"],
        json!([
            {"type": "output_text", "text": "again", "annotations": []},
            {"type": "refusal", "refusal": "No."},
            {"type": "output_text", "text": "again", "annotations": []},
            {"type": "refusal", "refusal": "Cannot comply."},
        ])
    );
}

#[test]
fn malformed_completions_and_unrepresentable_content_are_upstream_errors() {
    let mut sources = vec![
        Value::Null,
        json!([]),
        json!({}),
        json!({"choices": null}),
        json!({"choices": []}),
        json!({"choices": [null]}),
        json!({"choices": [{}, {}]}),
    ];
    for content in [
        Value::Null,
        json!(""),
        json!([]),
        json!({}),
        json!(123),
        json!([{"type": "image_url"}]),
        json!([{"type": "text"}]),
        json!([{"type": "refusal", "refusal": ""}]),
    ] {
        sources.push(completion(content, "stop"));
    }
    for (key, value) in [
        ("id", json!(1)),
        ("id", json!("")),
        ("created", json!(true)),
        ("created", json!(-1)),
        ("created", json!(1.5)),
        ("created", Value::Null),
        ("object", json!("chat.completion.chunk")),
        ("error", json!({"message": "failed"})),
    ] {
        let mut source = completion(json!("x"), "stop");
        source[key] = value;
        sources.push(source);
    }
    for (key, value) in [
        ("role", json!("user")),
        ("refusal", json!(1)),
        ("refusal", json!(" ")),
        (
            "function_call",
            json!({"name": "legacy", "arguments": "{}"}),
        ),
        ("audio", json!({"data": "AAAA"})),
        ("reasoning_content", json!("opaque thought")),
        ("reasoning", json!({"content": "opaque"})),
        ("annotations", json!([{"type": "citation"}])),
    ] {
        let mut source = completion(json!("x"), "stop");
        source["choices"][0]["message"][key] = value;
        sources.push(source);
    }
    for finish in [
        Value::Null,
        json!(""),
        json!("function_call"),
        json!("unknown"),
        json!(1),
    ] {
        let mut source = completion(json!("x"), "stop");
        source["choices"][0]["finish_reason"] = finish;
        sources.push(source);
    }
    for index in [json!(true), json!(-1), json!(1), json!("0")] {
        let mut source = completion(json!("x"), "stop");
        source["choices"][0]["index"] = index;
        sources.push(source);
    }
    for source in sources {
        assert_error(chat_to_response(&source, &json!({"model": "m"})), 502);
    }
    assert_error(
        chat_to_response(&completion(json!("x"), "stop"), &json!({})),
        400,
    );
}

#[test]
fn malformed_completion_tool_calls_are_not_repaired() {
    for calls in [
        json!({}),
        json!("calls"),
        json!([null]),
        json!([{}]),
        json!([{"id": "a", "type": "custom", "function": {}}]),
        json!([
            chat_call("a", "lookup", "{}"),
            chat_call("a", "other", "{}")
        ]),
    ] {
        let mut source = completion(Value::Null, "tool_calls");
        source["choices"][0]["message"]["tool_calls"] = calls;
        assert_error(chat_to_response(&source, &json!({"model": "m"})), 502);
    }
    for (pointer, value) in [
        ("/id", json!("")),
        ("/id", json!(1)),
        ("/function", Value::Null),
        ("/function/name", json!("")),
        ("/function/arguments", json!({})),
        ("/function/arguments", Value::Null),
    ] {
        let mut call = chat_call("a", "lookup", "{}");
        *call.pointer_mut(pointer).unwrap() = value;
        let mut source = completion(Value::Null, "tool_calls");
        source["choices"][0]["message"]["tool_calls"] = json!([call]);
        assert_error(chat_to_response(&source, &json!({"model": "m"})), 502);
    }
}

#[test]
fn buffered_text_event_order_and_snapshots_match_python() {
    let response = convert(&completion(json!("Hello"), "stop"));
    let original = response.clone();
    let mut events = response_events(&response).unwrap();
    assert_sequences(&events);
    assert_eq!(
        names(&events),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    for event in &events[..2] {
        assert_eq!(data(event)["response"]["id"], response["id"]);
        assert_eq!(data(event)["response"]["status"], "in_progress");
        assert_eq!(data(event)["response"]["output"], json!([]));
        assert_eq!(data(event)["response"]["output_text"], "");
        assert!(data(event)["response"]["usage"].is_null());
    }
    assert_eq!(data(&events[2])["item"]["content"], json!([]));
    assert_eq!(
        data(&events[3])["part"],
        json!({"type": "output_text", "text": "", "annotations": []})
    );
    assert_eq!(data(&events[4])["delta"], "Hello");
    assert_eq!(data(&events[4])["logprobs"], json!([]));
    assert_eq!(data(&events[5])["text"], "Hello");
    assert_eq!(data(&events[8])["response"], response);
    assert_eq!(events, response_events(&response).unwrap());
    events[0].data.as_mut().unwrap()["response"]["metadata"]["changed"] = json!("first only");
    assert!(
        data(&events[1])["response"]["metadata"]
            .get("changed")
            .is_none()
    );
    events[3].data.as_mut().unwrap()["part"]["annotations"]
        .as_array_mut()
        .unwrap()
        .push(json!({"changed": true}));
    assert_eq!(data(&events[6])["part"]["annotations"], json!([]));
    events[8].data.as_mut().unwrap()["response"]["output"][0]["content"][0]["text"] =
        json!("changed");
    assert_eq!(data(&events[7])["item"]["content"][0]["text"], "Hello");
    assert_eq!(response, original);
}

#[test]
fn buffered_tool_and_refusal_events_preserve_positions_and_bytes() {
    let arguments = "{ \"key\": \"\\u2603\" }\n";
    let mut source = completion(
        json!([
            {"type": "text", "text": "First"}, {"type": "refusal", "refusal": "No."},
            {"type": "text", "text": "Last"},
        ]),
        "tool_calls",
    );
    source["choices"][0]["message"]["tool_calls"] = json!([
        chat_call("a", "lookup", arguments),
        chat_call("b", "other", ""),
    ]);
    let response = convert(&source);
    let events = response_events(&response).unwrap();
    assert_sequences(&events);
    let starts: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "response.output_item.added")
        .collect();
    assert_eq!(
        starts
            .iter()
            .map(|event| data(event)["output_index"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(data(starts[1])["item"]["call_id"], "a");
    assert_eq!(data(starts[1])["item"]["arguments"], "");
    let deltas: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "response.function_call_arguments.delta")
        .collect();
    assert_eq!(data(deltas[0])["delta"], arguments);
    assert_eq!(data(deltas[1])["delta"], "");
    assert!(data(deltas[0]).get("content_index").is_none());
    assert_eq!(data(deltas[0])["item_id"], response["output"][1]["id"]);
    let refusal = events
        .iter()
        .find(|event| event.kind == "response.refusal.delta")
        .unwrap();
    assert_eq!(data(refusal)["content_index"], 1);
    assert_eq!(data(refusal)["delta"], "No.");
    assert!(data(refusal).get("logprobs").is_none());
}

#[test]
fn buffered_replay_validates_every_item_before_returning_any_events() {
    for (pointer, value) in [
        ("/id", json!("")),
        ("/object", json!("chat.completion")),
        ("/status", json!("failed")),
        ("/model", json!("")),
        ("/created_at", json!(-1)),
        ("/created_at", json!(true)),
        ("/error", json!({"message": "failure"})),
        (
            "/incomplete_details",
            json!({"reason": "max_output_tokens"}),
        ),
        ("/output", Value::Null),
        ("/output", json!([null])),
        ("/output_text", json!("different")),
        ("/output/0/id", json!("")),
        ("/output/0/status", json!("in_progress")),
        ("/output/0/status", json!("incomplete")),
        ("/output/0/role", json!("user")),
        ("/output/0/type", json!("reasoning")),
        ("/output/0/content", Value::Null),
        (
            "/output/0/content",
            json!([{"type": "input_text", "text": "x"}]),
        ),
        (
            "/output/0/content",
            json!([{"type": "output_text", "text": 1}]),
        ),
    ] {
        let mut response = convert(&completion(json!("Hello"), "stop"));
        *response.pointer_mut(pointer).unwrap() = value;
        assert_error(response_events(&response), 502);
    }

    let mut response = convert(&completion(json!("Hello"), "stop"));
    let duplicate = response["output"][0].clone();
    response["output"].as_array_mut().unwrap().push(duplicate);
    assert_error(response_events(&response), 502);
    for details in [Value::Null, json!({"reason": "unknown"})] {
        let mut response = convert(&completion(Value::Null, "length"));
        response["incomplete_details"] = details;
        assert_error(response_events(&response), 502);
    }
    let mut response = convert(&completion(json!("Hello"), "stop"));
    response["output"] = json!([]);
    response["output_text"] = json!("");
    assert_error(response_events(&response), 502);
}

mod stream_and_anthropic {
    use super::*;

    #[test]
    fn streaming_text_deltas_precede_terminal_and_snapshots_do_not_alias() {
        let mut codec = stream();
        let mut events = codec.start().unwrap();
        events.extend(
            codec
                .feed(&chunk(json!({"role": "assistant", "content": "Hel"}), None))
                .unwrap(),
        );
        let snapshot = events.clone();
        assert!(names(&events).contains(&"response.output_text.delta"));
        assert!(codec.response().is_none());
        events.extend(
            codec
                .feed(&chunk(json!({"content": "lo"}), Some("stop")))
                .unwrap(),
        );
        assert!(codec.response().is_none(), "a finish reason is not [DONE]");
        events.extend(
            codec
                .feed(&json!({
                    "choices": [], "usage": {
                        "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12,
                        "prompt_tokens_details": {"cached_tokens": 3},
                        "completion_tokens_details": {"reasoning_tokens": 1},
                    },
                }))
                .unwrap(),
        );
        events.extend(codec.finish().unwrap());
        assert_eq!(&events[..snapshot.len()], snapshot.as_slice());
        assert_sequences(&events);
        assert_eq!(codec.response().unwrap()["output_text"], "Hello");
        assert_eq!(
            codec.response().unwrap()["usage"]["input_tokens_details"],
            json!({"cached_tokens": 3})
        );
        assert_eq!(
            codec.response().unwrap()["usage"]["output_tokens_details"],
            json!({"reasoning_tokens": 1})
        );
        assert_eq!(events.last().unwrap().kind, "response.completed");
        events.last_mut().unwrap().data.as_mut().unwrap()["response"]["output"][0]["content"][0]
            ["text"] = json!("changed");
        let mut response = codec.response().unwrap();
        response["output"][0]["content"][0]["text"] = json!("also changed");
        assert_eq!(
            codec.response().unwrap()["output"][0]["content"][0]["text"],
            "Hello"
        );
    }

    #[test]
    fn interim_stream_usage_is_validated_but_not_mistaken_for_final_usage() {
        let mut codec = stream();
        codec.start().unwrap();
        for text in ["again", "again"] {
            let mut value = chunk(json!({"content": text}), None);
            value["usage"] = json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0});
            let events = codec.feed(&value).unwrap();
            assert!(names(&events).contains(&"response.output_text.delta"));
        }
        let mut terminal = chunk(json!({}), Some("stop"));
        terminal["usage"] = json!({
            "prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 173, "reasoning_tokens": 157,
        });
        codec.feed(&terminal).unwrap();
        codec.finish().unwrap();
        assert_eq!(codec.response().unwrap()["output_text"], "againagain");
        assert_eq!(
            codec.response().unwrap()["usage"],
            json!({
                "input_tokens": 10, "output_tokens": 163, "total_tokens": 173,
                "output_tokens_details": {"reasoning_tokens": 157},
            })
        );

        let mut codec = stream();
        codec.start().unwrap();
        let mut interim = chunk(json!({"content": "Hello"}), None);
        interim["usage"] = json!({"prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11});
        codec.feed(&interim).unwrap();
        codec.feed(&chunk(json!({}), Some("stop"))).unwrap();
        codec.finish().unwrap();
        assert!(codec.response().unwrap()["usage"].is_null());
    }

    #[test]
    fn interleaved_parallel_tool_identity_and_argument_fragments_are_lossless() {
        let mut codec = stream();
        let mut events = codec.start().unwrap();
        events.extend(codec.feed(&chunk(json!({"tool_calls": [
            {"index": 0, "id": "call_", "type": "function", "function": {"name": "lo", "arguments": ""}},
            {"index": 1, "id": "call_second", "type": "function", "function": {"name": "search", "arguments": "{"}},
        ]}), None)).unwrap());
        let prior = events.clone();
        let incremental = codec.feed(&chunk(json!({"tool_calls": [
            {"index": 0, "id": "first", "function": {"name": "okup", "arguments": "{\"id\":"}},
            {"index": 1, "function": {"arguments": "\"q\":\"x\"}"}},
        ]}), None)).unwrap();
        assert!(names(&incremental).contains(&"response.function_call_arguments.delta"));
        events.extend(incremental);
        events.extend(
            codec
                .feed(&chunk(
                    json!({"tool_calls": [
                        {"index": 0, "function": {"arguments": "\"42\"}"}},
                    ]}),
                    Some("tool_calls"),
                ))
                .unwrap(),
        );
        events.extend(codec.finish().unwrap());
        assert_eq!(&events[..prior.len()], prior.as_slice());
        assert_sequences(&events);
        let response = codec.response().unwrap();
        let output = response["output"].as_array().unwrap();
        assert_eq!(output[0]["call_id"], "call_first");
        assert_eq!(output[1]["call_id"], "call_second");
        assert_eq!(output[0]["name"], "lookup");
        assert_eq!(output[1]["name"], "search");
        assert_eq!(output[0]["arguments"], "{\"id\":\"42\"}");
        assert_eq!(output[1]["arguments"], "{\"q\":\"x\"}");
        for (index, item) in output.iter().enumerate() {
            let fragments: String = events
                .iter()
                .filter(|event| {
                    event.kind == "response.function_call_arguments.delta"
                        && data(event)["output_index"] == json!(index)
                })
                .map(|event| data(event)["delta"].as_str().unwrap())
                .collect();
            assert_eq!(fragments, item["arguments"].as_str().unwrap());
            let added: Vec<_> = events
                .iter()
                .filter(|event| {
                    event.kind == "response.output_item.added"
                        && data(event)["output_index"] == json!(index)
                })
                .collect();
            assert_eq!(added.len(), 1);
            assert_eq!(data(added[0])["item"]["id"], item["id"]);
            assert_eq!(data(added[0])["item"]["call_id"], item["call_id"]);
        }
    }

    #[test]
    fn buffered_arguments_wait_for_identity_and_empty_arguments_announce_at_finish() {
        let mut codec = stream();
        codec.start().unwrap();
        assert!(
            codec
                .feed(&chunk(
                    json!({"tool_calls": [
                        {"index": 0, "function": {"arguments": "{\"id\":"}},
                    ]}),
                    None
                ))
                .unwrap()
                .is_empty()
        );
        let events = codec
            .feed(&chunk(
                json!({"tool_calls": [
                    {"index": 0, "id": "call_late", "function": {"name": "lookup"}},
                ]}),
                None,
            ))
            .unwrap();
        assert_eq!(
            names(&events),
            [
                "response.output_item.added",
                "response.function_call_arguments.delta"
            ]
        );
        assert_eq!(data(&events[1])["delta"], "{\"id\":");
        codec.feed(&chunk(json!({"tool_calls": [
            {"index": 0, "id": "call_late", "function": {"name": "lookup", "arguments": "42}"}},
        ]}), Some("tool_calls"))).unwrap();
        codec.finish().unwrap();
        assert_eq!(
            codec.response().unwrap()["output"][0]["arguments"],
            "{\"id\":42}"
        );
        let mut codec = stream();
        codec.start().unwrap();
        assert!(codec.feed(&chunk(json!({"tool_calls": [
            {"index": 0, "id": "call_empty", "function": {"name": "noop", "arguments": ""}},
        ]}), Some("tool_calls"))).unwrap().is_empty());
        let events = codec.finish().unwrap();
        assert_eq!(events[0].kind, "response.output_item.added");
        assert_eq!(data(&events[0])["item"]["status"], "in_progress");
        assert_eq!(codec.response().unwrap()["output"][0]["arguments"], "");
    }

    #[test]
    fn text_and_tool_deltas_keep_arrival_order_without_reordering_output_slots() {
        let mut codec = stream();
        let mut events = codec.start().unwrap();
        events.extend(
            codec
                .feed(&chunk(
                    json!({"tool_calls": [
                        {"index": 0, "function": {"arguments": "{"}},
                    ]}),
                    None,
                ))
                .unwrap(),
        );
        let text = codec.feed(&chunk(json!({"content": "one"}), None)).unwrap();
        assert_eq!(data(&text[0])["output_index"], 1);
        events.extend(text);
        let tool = codec.feed(&chunk(json!({"tool_calls": [
            {"index": 0, "id": "call_first", "function": {"name": "lookup", "arguments": "}"}},
        ]}), None)).unwrap();
        assert_eq!(data(&tool[0])["output_index"], 0);
        events.extend(tool);
        let text = codec
            .feed(&chunk(json!({"content": "one"}), Some("tool_calls")))
            .unwrap();
        assert_eq!(names(&text), ["response.output_text.delta"]);
        assert_eq!(data(&text[0])["output_index"], 1);
        events.extend(text);
        events.extend(codec.finish().unwrap());
        assert_sequences(&events);
        let response = codec.response().unwrap();
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][1]["type"], "message");
        assert_eq!(response["output_text"], "oneone");
        let observed: Vec<_> = events
            .iter()
            .filter(|event| {
                event.kind == "response.output_text.delta"
                    || event.kind == "response.function_call_arguments.delta"
            })
            .map(|event| data(event)["delta"].as_str().unwrap())
            .collect();
        assert_eq!(observed, ["one", "{}", "one"]);
    }

    #[test]
    fn stream_refusal_text_switches_and_incomplete_outcomes_are_explicit() {
        for (finish, status, reason) in [
            ("stop", "completed", Value::Null),
            (
                "length",
                "incomplete",
                json!({"reason": "max_output_tokens"}),
            ),
            (
                "content_filter",
                "incomplete",
                json!({"reason": "content_filter"}),
            ),
        ] {
            let mut codec = stream();
            codec.start().unwrap();
            codec
                .feed(&chunk(json!({"content": "Explanation."}), None))
                .unwrap();
            let events = codec
                .feed(&chunk(json!({"refusal": "Cannot "}), None))
                .unwrap();
            assert_eq!(events.last().unwrap().kind, "response.refusal.delta");
            codec
                .feed(&chunk(json!({"refusal": "answer"}), None))
                .unwrap();
            codec
                .feed(&chunk(json!({"content": "Explanation."}), Some(finish)))
                .unwrap();
            let terminal = codec.finish().unwrap();
            let response = codec.response().unwrap();
            assert_eq!(terminal.last().unwrap().kind, format!("response.{status}"));
            assert_eq!(
                response["output"][0]["content"],
                json!([
                    {"type": "output_text", "text": "Explanation.", "annotations": []},
                    {"type": "refusal", "refusal": "Cannot answer"},
                    {"type": "output_text", "text": "Explanation.", "annotations": []},
                ])
            );
            assert_eq!(response["output_text"], "Explanation.Explanation.");
            assert!(response["usage"].is_null());
            assert_eq!(response["incomplete_details"], reason);
        }
    }

    #[test]
    fn stream_requires_start_finish_reason_and_single_terminal() {
        let mut codec = stream();
        assert_error(codec.feed(&chunk(json!({"content": "early"}), None)), 502);
        assert_error(codec.finish(), 502);
        codec.start().unwrap();
        codec
            .feed(&chunk(json!({"content": "partial"}), None))
            .unwrap();
        let error = codec.finish().unwrap_err();
        assert!(error.message.contains("finish reason"));
        assert!(codec.response().is_none());
        codec.feed(&chunk(json!({}), Some("stop"))).unwrap();
        codec.finish().unwrap();
        assert_error(codec.finish(), 502);
        assert_error(codec.feed(&chunk(json!({"content": "late"}), None)), 502);
        assert_error(codec.start(), 502);
    }

    #[test]
    fn malformed_stream_chunks_fail_closed_without_a_success_response() {
        for value in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"choices": []}),
            json!("[DONE]"),
            chunk(json!({"reasoning_content": "opaque thought"}), None),
            chunk(json!({"reasoning": {"encrypted_content": "opaque"}}), None),
            chunk(json!({"content": 123}), None),
            chunk(json!({"content": "text"}), Some("unknown")),
            chunk(json!({"tool_calls": [{"index": -1}]}), None),
            chunk(json!({"tool_calls": [{"index": true}]}), None),
            chunk(
                json!({"tool_calls": [{"index": 0, "function": null}]}),
                None,
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "type": "custom"}]}),
                None,
            ),
            json!({"choices": [], "usage": {"prompt_tokens": -1, "completion_tokens": 1}}),
            json!({"choices": [{"index": 1, "delta": {"content": "x"}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "logprobs": []}]}),
            json!({"choices": [{"index": 0, "delta": {}}, {"index": 1, "delta": {}}]}),
            json!({"error": {"message": "upstream failed"}, "choices": []}),
        ] {
            let mut codec = stream();
            codec.start().unwrap();
            assert_error(codec.feed(&value), 502);
            assert_error(
                codec.feed(&chunk(json!({"content": "recover"}), Some("stop"))),
                502,
            );
            assert_error(codec.finish(), 502);
            assert!(codec.response().is_none());
        }
    }

    #[test]
    fn missing_tool_identity_gaps_duplicates_and_empty_success_fail_on_finish() {
        for calls in [
            json!([{"index": 0, "function": {"arguments": "{}"}}]),
            json!([{"index": 1, "id": "call_1", "function": {"name": "tool", "arguments": "{}"}}]),
            json!([
                {"index": 0, "id": "duplicate", "function": {"name": "tool", "arguments": "{}"}},
                {"index": 1, "id": "duplicate", "function": {"name": "tool", "arguments": "{}"}},
            ]),
            json!([]),
        ] {
            let mut codec = stream();
            codec.start().unwrap();
            codec
                .feed(&chunk(json!({"tool_calls": calls}), Some("tool_calls")))
                .unwrap();
            assert_error(codec.finish(), 502);
            assert!(codec.response().is_none());
        }
        let mut codec = stream();
        codec.start().unwrap();
        codec.feed(&chunk(json!({}), Some("stop"))).unwrap();
        assert_error(codec.finish(), 502);
        for finish in ["length", "content_filter"] {
            let mut codec = stream();
            codec.start().unwrap();
            codec.feed(&chunk(json!({}), Some(finish))).unwrap();
            assert_eq!(
                codec.finish().unwrap().last().unwrap().kind,
                "response.incomplete"
            );
        }
    }

    #[test]
    fn published_identity_and_final_usage_cannot_change() {
        let mut codec = stream();
        codec.start().unwrap();
        codec
            .feed(&chunk(
                json!({"tool_calls": [
                    {"index": 0, "id": "call_one", "function": {"name": "tool", "arguments": "{"}},
                ]}),
                None,
            ))
            .unwrap();
        let error = codec
            .feed(&chunk(
                json!({"tool_calls": [{"index": 0, "id": "_changed"}]}),
                None,
            ))
            .unwrap_err();
        assert!(error.message.contains("identity changed"));
        assert_error(codec.finish(), 502);
        let mut codec = stream();
        codec.start().unwrap();
        codec.feed(&chunk(json!({"content": "x"}), None)).unwrap();
        let mut changed = chunk(json!({}), Some("stop"));
        changed["id"] = json!("another_completion");
        assert_error(codec.feed(&changed), 502);
        let mut codec = stream();
        codec.start().unwrap();
        let usage = json!({"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 1}});
        codec.feed(&usage).unwrap();
        codec.feed(&usage).unwrap();
        let error = codec
            .feed(&json!({"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 2}}))
            .unwrap_err();
        assert!(error.message.contains("conflicting"));
        let mut codec = stream();
        codec.start().unwrap();
        codec
            .feed(&chunk(json!({"content": "x"}), Some("stop")))
            .unwrap();
        assert_error(codec.feed(&chunk(json!({}), Some("stop"))), 502);
        assert_error(codec.finish(), 502);
    }

    #[test]
    fn stream_ids_are_injectable_unique_and_owned_request_is_immutable() {
        assert_error(
            ChatResponseStream::new(&json!({"model": "m"}), Some(" ")),
            400,
        );
        assert_error(ChatResponseStream::new(&json!({}), None), 400);
        let mut request = json!({"model": "original", "metadata": {"key": "value"}});
        let mut first = ChatResponseStream::new(&request, None).unwrap();
        let mut second = ChatResponseStream::new(&request, None).unwrap();
        request["model"] = json!("changed");
        request["metadata"]["key"] = json!("changed");
        let first = first.start().unwrap();
        let second = second.start().unwrap();
        assert_ne!(
            data(&first[0])["response"]["id"],
            data(&second[0])["response"]["id"]
        );
        assert_eq!(data(&first[0])["response"]["model"], "original");
        assert_eq!(data(&first[0])["response"]["metadata"]["key"], "value");
        let injected = stream().start().unwrap();
        assert_eq!(data(&injected[0])["response"]["id"], "resp_fixture");
    }

    #[test]
    fn anthropic_mixed_content_preserves_attachments_text_and_tool_boundaries() {
        let payload = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "Read this"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aW1hZ2U="}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.invalid/image.png"}},
                    {"type": "document", "title": "fixture.pdf", "source": {"type": "base64", "media_type": "application/pdf", "data": "cGRm"}},
                ]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Looking up"},
                    {"type": "tool_use", "id": "call_1", "name": "lookup", "input": {"id": 42}},
                    {"type": "tool_use", "id": "call_2", "name": "other", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_2", "content": "second"},
                    {"type": "tool_result", "tool_use_id": "call_1", "is_error": true, "content": [{"type": "text", "text": "Not found"}]},
                    {"type": "text", "text": "Try again"}, {"type": "text", "text": "Please."},
                ]},
            ],
        });
        let original = payload.clone();
        assert_eq!(anthropic::validate_request(&payload).unwrap(), json!({}));
        let request = anthropic::to_responses(&payload, "native", None, None).unwrap();
        assert_eq!(
            request["input"],
            json!([
                {"role": "user", "content": [
                    {"type": "input_text", "text": "Read this"},
                    {"type": "input_image", "image_url": "data:image/png;base64,aW1hZ2U="},
                    {"type": "input_image", "image_url": "https://example.invalid/image.png"},
                    {"type": "input_file", "file_data": "data:application/pdf;base64,cGRm", "filename": "fixture.pdf"},
                ]},
                {"role": "assistant", "content": "Looking up"},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"id\":42}"},
                {"type": "function_call", "call_id": "call_2", "name": "other", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_2", "output": "second"},
                {"type": "function_call_output", "call_id": "call_1", "output": "Error: Not found"},
                {"role": "user", "content": "Try again\nPlease."},
            ])
        );
        assert_eq!(payload, original);
        assert_error(responses_to_chat(&request, false), 400);
    }

    #[test]
    fn anthropic_parallel_tool_history_can_continue_through_chat_without_losing_calls() {
        let payload = json!({"messages": [
            {"role": "user", "content": "Look up."},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "a", "name": "lookup", "input": {"id": 1}},
                {"type": "tool_use", "id": "b", "name": "lookup", "input": {"id": 2}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "b", "content": "B"},
                {"type": "tool_result", "tool_use_id": "a", "content": "A"},
                {"type": "text", "text": "Continue."},
            ]},
        ]});
        anthropic::validate_request(&payload).unwrap();
        let request = anthropic::to_responses(&payload, "chat-model", None, None).unwrap();
        let chat = responses_to_chat(&request, false).unwrap();
        assert_eq!(chat["messages"][1]["tool_calls"][0]["id"], "a");
        assert_eq!(chat["messages"][1]["tool_calls"][1]["id"], "b");
        assert_eq!(chat["messages"][2]["tool_call_id"], "b");
        assert_eq!(chat["messages"][3]["tool_call_id"], "a");
        assert_eq!(chat["messages"][4]["content"], "Continue.");
    }

    #[test]
    fn anthropic_effort_caps_schema_and_requested_model_are_exact() {
        let payload = json!({
            "model": "client-alias", "messages": [{"role": "user", "content": "hello"}],
            "system": [{"type": "text", "text": "First"}, {"type": "text", "text": ""}, {"type": "text", "text": "Second"}],
            "max_tokens": 150_000, "stream": true,
            "output_config": {"effort": "high", "format": {"type": "json_schema", "name": "result", "schema": {"type": "object"}}},
        });
        let original = payload.clone();
        for (cap, expected) in [(None, 150_000), (Some(100), 100), (Some(200_000), 150_000)] {
            let result = anthropic::to_responses(&payload, "personal-model", None, cap).unwrap();
            assert_eq!(result["model"], "personal-model");
            assert_eq!(result["instructions"], "First\nSecond");
            assert_eq!(result["max_output_tokens"], expected);
            assert_eq!(result["reasoning"], json!({"effort": "high"}));
            assert_eq!(result["stream"], false);
            assert_eq!(
                result["text"]["format"],
                json!({
                    "type": "json_schema", "name": "result", "schema": {"type": "object"}, "strict": true,
                })
            );
        }
        for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            assert_eq!(
                anthropic::to_responses(&payload, "m", Some(effort), None).unwrap()["reasoning"]["effort"],
                effort
            );
        }
        assert_error(
            anthropic::to_responses(&payload, "m", Some("invalid"), None),
            400,
        );
        assert_error(anthropic::to_responses(&payload, "m", None, Some(0)), 400);
        let no_maximum =
            anthropic::to_responses(&anthropic_request(), "m", None, Some(100)).unwrap();
        assert!(no_maximum.get("max_output_tokens").is_none());
        assert_eq!(payload, original);
    }

    #[test]
    fn anthropic_tool_choices_and_extras_are_mapped_without_implicit_personal_policy() {
        for (choice, expected) in [
            (json!({"type": "auto"}), json!("auto")),
            (json!({"type": "any"}), json!("required")),
            (json!({"type": "none"}), json!("none")),
            (
                json!({"type": "tool", "name": "lookup"}),
                json!({"type": "function", "name": "lookup"}),
            ),
        ] {
            let mut payload = anthropic_request();
            payload["tool_choice"] = choice;
            payload["tools"] = json!([{"name": "lookup", "input_schema": {"type": "object"}, "description": "Find"}]);
            let result = anthropic::to_responses(&payload, "m", None, None).unwrap();
            assert_eq!(result["tool_choice"], expected);
            assert_eq!(
                result["tools"],
                json!([{
                    "type": "function", "name": "lookup", "description": "Find",
                    "parameters": {"type": "object"}, "strict": false,
                }])
            );
        }
        let mut payload = anthropic_request();
        payload["temperature"] = json!(0.5);
        payload["top_p"] = json!(0.9);
        payload["thinking"] = json!({"type": "adaptive"});
        payload["output_config"] = json!({"effort": "high"});
        payload["tool_choice"] = json!({"type": "auto", "disable_parallel_tool_use": true});
        let original = payload.clone();
        let extras = anthropic::validate_request(&payload).unwrap();
        assert_eq!(
            extras,
            json!({"temperature": 0.5, "top_p": 0.9, "parallel_tool_calls": false})
        );
        let mut request = anthropic::to_responses(&payload, "m", None, None).unwrap();
        assert!(request.get("temperature").is_none());
        request
            .as_object_mut()
            .unwrap()
            .extend(extras.as_object().unwrap().clone());
        let chat = responses_to_chat(&request, false).unwrap();
        assert_eq!(chat["temperature"], 0.5);
        assert_eq!(chat["top_p"], 0.9);
        assert_eq!(chat["parallel_tool_calls"], false);
        assert_eq!(payload, original);
        payload["thinking"] = json!({"type": "enabled", "budget_tokens": 1000});
        assert_error(anthropic::validate_request(&payload), 400);
        assert!(anthropic::to_responses(&payload, "legacy", Some("high"), Some(100)).is_ok());
    }

    #[test]
    fn anthropic_sdk_beta_context_preserve_all_and_adaptive_effort_are_accepted() {
        for context in [
            Value::Null,
            json!({}),
            json!({"edits": []}),
            json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]}),
        ] {
            let mut payload = anthropic_request();
            payload["thinking"] = json!({"type": "adaptive", "display": "omitted"});
            payload["output_config"] = json!({"effort": "high"});
            payload["context_management"] = context;
            let original = payload.clone();
            assert_eq!(anthropic::validate_request(&payload).unwrap(), json!({}));
            assert_eq!(payload, original);
        }
        for context in [
            json!([]),
            json!({"edits": null}),
            json!({"unexpected": true}),
            json!({"edits": [{"type": "clear_thinking_20251015", "keep": 0}]}),
            json!({"edits": [{"type": "clear_tool_uses_20250919"}]}),
            json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all", "extra": true}]}),
            json!({"edits": ["clear"]}),
        ] {
            let mut payload = anthropic_request();
            payload["context_management"] = context;
            assert_error(anthropic::validate_request(&payload), 400);
        }
    }

    #[test]
    fn anthropic_invalid_and_unrepresentable_requests_are_explicit_errors() {
        for payload in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"messages": []}),
            json!({"messages": null}),
        ] {
            assert_error(anthropic::validate_request(&payload), 400);
        }
        for (key, value) in [
            ("messages", json!([{"role": [], "content": "hello"}])),
            (
                "messages",
                json!([{"role": "assistant", "content": [{"type": "thinking", "thinking": "opaque"}]}]),
            ),
            (
                "messages",
                json!([{"role": "user", "content": [{"type": [], "text": "hello"}]}]),
            ),
            (
                "messages",
                json!([{"role": "user", "content": [{"type": "text", "text": 1}]}]),
            ),
            (
                "messages",
                json!([{"role": "user", "content": [{"type": "tool_use", "id": "a", "name": "f", "input": {}}]}]),
            ),
            (
                "messages",
                json!([{"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "f", "input": []}]}]),
            ),
            (
                "messages",
                json!([{"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "is_error": 1}]}]),
            ),
            (
                "messages",
                json!([{"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": [{"type": "image"}]}]}]),
            ),
            (
                "messages",
                json!([{"role": "user", "content": [{"type": "image", "source": {"type": "url", "url": ""}}]}]),
            ),
            (
                "messages",
                json!([{"role": "user", "content": [{"type": "document", "source": {"type": "url", "url": "https://example.invalid/a.pdf"}}]}]),
            ),
            (
                "messages",
                json!([{"role": "user", "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": ""}}]}]),
            ),
            ("system", Value::Null),
            ("system", json!([{"type": "image"}])),
            ("max_tokens", json!(0)),
            ("max_tokens", json!(-1)),
            ("max_tokens", json!(true)),
            ("max_tokens", json!(1.5)),
            ("max_tokens", json!("100")),
            ("stream", json!(1)),
            ("stream", Value::Null),
            ("tools", Value::Null),
            (
                "tools",
                json!([{"type": [], "name": "tool", "input_schema": {}}]),
            ),
            (
                "tools",
                json!([{"type": "web_search", "name": "tool", "input_schema": {}}]),
            ),
            ("tools", json!([{"name": "tool", "input_schema": []}])),
            ("tool_choice", json!({"type": "invalid"})),
            ("tool_choice", json!({"type": "tool", "name": ""})),
            (
                "tool_choice",
                json!({"type": "auto", "disable_parallel_tool_use": 1}),
            ),
            ("output_config", Value::Null),
            ("output_config", json!({"effort": []})),
            ("output_config", json!({"unknown": true})),
            (
                "output_config",
                json!({"format": {"type": "xml", "schema": {}}}),
            ),
            (
                "output_config",
                json!({"format": {"type": "json_schema", "schema": []}}),
            ),
            (
                "thinking",
                json!({"type": "enabled", "budget_tokens": 1000}),
            ),
            ("thinking", json!({"type": "adaptive"})),
            ("thinking", json!("disabled")),
            ("top_k", json!(0)),
            ("stop_sequences", json!(["STOP"])),
            ("service_tier", json!({})),
            ("service_tier", json!("priority")),
            ("temperature", json!(true)),
            ("temperature", Value::Null),
            ("temperature", parse("1e999")),
            ("top_p", json!("0.9")),
            ("unknown_control", json!(false)),
        ] {
            let mut payload = anthropic_request();
            payload[key] = value;
            assert_error(anthropic::validate_request(&payload), 400);
        }
    }

    #[test]
    fn anthropic_tool_json_retains_python_ascii_escaping_and_input_key_order() {
        // This fixture intentionally requires serde_json's preserve_order feature.
        let payload = parse(
            r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"lookup","input":{"z":"雪😀","a":1.0,"small":1e-5}}]}]}"#,
        );
        let original = payload.clone();
        let result = anthropic::to_responses(&payload, "m", None, None).unwrap();
        assert_eq!(
            result["input"][0]["arguments"],
            r#"{"z":"\u96ea\ud83d\ude00","a":1.0,"small":1e-05}"#
        );
        assert_eq!(payload, original);
    }

    #[test]
    fn anthropic_validated_response_normalizes_only_missing_cache_details() {
        let mut response = native_response();
        response["extension"] = json!({"opaque": "preserve"});
        response["usage"]["extra"] = json!("preserve");
        let original = response.clone();
        let validated = anthropic::validate_response(&response).unwrap();
        assert_eq!(validated["usage"]["input_tokens_details"], json!({}));
        assert_eq!(validated["extension"], response["extension"]);
        assert_eq!(validated["usage"]["extra"], "preserve");
        assert_eq!(response, original);
        assert_eq!(anthropic::validate_response(&validated).unwrap(), validated);
    }

    #[test]
    fn anthropic_missing_invalid_and_contradictory_usage_is_never_fabricated() {
        for usage in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"input_tokens": -1, "output_tokens": 2}),
            json!({"input_tokens": 1, "output_tokens": true}),
            json!({"input_tokens": 1.0, "output_tokens": 2}),
            json!({"input_tokens": 1, "output_tokens": 2, "total_tokens": 4}),
            json!({"input_tokens": 1, "output_tokens": 2, "input_tokens_details": []}),
            json!({"input_tokens": 1, "output_tokens": 2, "input_tokens_details": {"cached_tokens": null}}),
            json!({"input_tokens": 1, "output_tokens": 2, "input_tokens_details": {"cache_write_tokens": -1}}),
            json!({"input_tokens": 1, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 1, "cache_write_tokens": 1}}),
            json!({"input_tokens": u64::MAX, "output_tokens": 2, "input_tokens_details": {"cached_tokens": u64::MAX, "cache_write_tokens": 1}}),
        ] {
            let mut response = native_response();
            response["usage"] = usage;
            assert_error(anthropic::validate_response(&response), 502);
            assert_error(anthropic::to_message(&response, "requested"), 502);
        }
        let mut response = native_response();
        response.as_object_mut().unwrap().remove("usage");
        assert_error(anthropic::to_message(&response, "requested"), 502);
    }

    #[test]
    fn anthropic_partial_non_object_and_non_finite_tool_inputs_are_never_repaired() {
        for arguments in [
            "",
            "{",
            "{\"key\":",
            "null",
            "[]",
            "\"text\"",
            "false",
            "1",
            "{\"value\":NaN}",
            "{\"value\":Infinity}",
            "{\"value\":1e999}",
            "{} trailing",
        ] {
            let mut response = native_response();
            response["status"] = json!("incomplete");
            response["incomplete_details"] = json!({"reason": "max_output_tokens"});
            response["output"] = json!([{"type": "function_call", "name": "tool", "call_id": "a", "arguments": arguments}]);
            assert_error(anthropic::validate_response(&response), 502);
            assert_error(anthropic::to_message(&response, "requested"), 502);
        }
        for (key, value) in [
            ("call_id", json!("")),
            ("name", json!("")),
            ("arguments", json!({})),
        ] {
            let mut response = native_response();
            response["output"] = json!([{"type": "function_call", "name": "tool", "call_id": "a", "arguments": "{}"}]);
            response["output"][0][key] = value;
            assert_error(anthropic::to_message(&response, "requested"), 502);
        }
    }

    #[test]
    fn anthropic_message_preserves_order_cache_usage_and_requested_model() {
        let response = json!({
            "id": "resp_fixture", "status": "completed",
            "output": [
                {"type": "reasoning", "encrypted_content": "opaque", "summary": []},
                {"type": "message", "content": [{"type": "output_text", "text": "First"}]},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"id\":42}"},
                {"type": "message", "content": [{"type": "output_text", "text": "Last"}]},
            ],
            "usage": {"input_tokens": 10, "output_tokens": 4, "input_tokens_details": {"cached_tokens": 3, "cache_write_tokens": 2}},
        });
        let original = response.clone();
        assert_eq!(
            anthropic::to_message(&response, "requested-model").unwrap(),
            json!({
                "id": "msg_fixture", "type": "message", "role": "assistant", "model": "requested-model",
                "content": [
                    {"type": "text", "text": "First"},
                    {"type": "tool_use", "id": "call_1", "name": "lookup", "input": {"id": 42}},
                    {"type": "text", "text": "Last"},
                ],
                "stop_reason": "tool_use", "stop_sequence": null,
                "usage": {"input_tokens": 5, "output_tokens": 4, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3},
            })
        );
        assert_eq!(response, original);
    }

    #[test]
    fn anthropic_content_filter_stop_is_refusal_not_a_fabricated_token_limit() {
        for content in [
            json!([]),
            json!([{"type": "message", "content": [{"type": "output_text", "text": "Partial"}]}]),
            json!([{"type": "message", "content": [{"type": "refusal", "refusal": "Cannot continue"}]}]),
        ] {
            let mut response = native_response();
            response["status"] = json!("incomplete");
            response["output"] = content;
            response["incomplete_details"] = json!({"reason": "content_filter"});
            let message = anthropic::to_message(&response, "requested-model").unwrap();
            assert_eq!(message["stop_reason"], "refusal");
            let events = anthropic::events(&message).unwrap();
            let terminal = events
                .iter()
                .find(|event| event.kind == "message_delta")
                .unwrap();
            assert_eq!(data(terminal)["delta"]["stop_reason"], "refusal");
            response["incomplete_details"] = json!({"reason": "max_output_tokens"});
            assert_eq!(
                anthropic::to_message(&response, "requested-model").unwrap()["stop_reason"],
                "max_tokens"
            );
        }
        let mut response = native_response();
        response["output"][0]["content"] = json!([{"type": "refusal", "refusal": "No."}]);
        assert_eq!(
            anthropic::to_message(&response, "requested").unwrap()["stop_reason"],
            "refusal"
        );
        response["status"] = json!("incomplete");
        response["incomplete_details"] = json!({"reason": "unknown"});
        assert_error(anthropic::to_message(&response, "requested"), 502);
    }

    #[test]
    fn anthropic_malformed_outputs_do_not_produce_success_messages() {
        for output in [
            Value::Null,
            json!([null]),
            json!([{"type": "image_generation_call"}]),
            json!([{"type": "message", "content": null}]),
            json!([{"type": "message", "content": [{"type": "output_text", "text": 1}]}]),
            json!([{"type": "message", "content": [{"type": "input_text", "text": "not output"}]}]),
            json!([
                {"type": "function_call", "name": "f", "call_id": "a", "arguments": "{}"},
                {"type": "function_call", "name": "g", "call_id": "a", "arguments": "{}"},
            ]),
        ] {
            let mut response = native_response();
            response["output"] = output;
            assert_error(anthropic::validate_response(&response), 502);
            assert_error(anthropic::to_message(&response, "m"), 502);
        }
        for status in ["failed", "in_progress", "cancelled"] {
            let mut response = native_response();
            response["status"] = json!(status);
            assert_error(anthropic::to_message(&response, "m"), 502);
        }
    }

    #[test]
    fn anthropic_buffered_events_match_sdk_shapes_and_keep_immutable_snapshots() {
        let mut response = native_response();
        response["output"].as_array_mut().unwrap().push(json!({
            "type": "function_call", "call_id": "a", "name": "lookup",
            "arguments": "{\"z\":\"雪😀\",\"a\":1}",
        }));
        let message = anthropic::to_message(&response, "requested").unwrap();
        let original = message.clone();
        let mut events = anthropic::events(&message).unwrap();
        assert_eq!(
            names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        for event in &events {
            assert_eq!(data(event)["type"], event.kind);
            assert!(data(event).get("sequence_number").is_none());
        }
        assert_eq!(data(&events[0])["message"]["content"], json!([]));
        assert!(data(&events[0])["message"]["stop_reason"].is_null());
        assert_eq!(data(&events[0])["message"]["usage"], message["usage"]);
        assert_eq!(
            data(&events[1])["content_block"],
            json!({"type": "text", "text": ""})
        );
        assert_eq!(
            data(&events[2])["delta"],
            json!({"type": "text_delta", "text": "Hello"})
        );
        assert_eq!(
            data(&events[4])["content_block"],
            json!({"type": "tool_use", "id": "a", "name": "lookup", "input": {}})
        );
        assert_eq!(
            data(&events[5])["delta"],
            json!({
                "type": "input_json_delta", "partial_json": "{\"z\":\"\\u96ea\\ud83d\\ude00\",\"a\":1}",
            })
        );
        assert_eq!(
            data(&events[7]),
            &json!({
                "type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                "usage": {"output_tokens": 4},
            })
        );
        events[0].data.as_mut().unwrap()["message"]["usage"]["output_tokens"] = json!(999);
        assert_eq!(data(&events[7])["usage"]["output_tokens"], 4);
        events[4].data.as_mut().unwrap()["content_block"]["input"]["changed"] = json!(true);
        assert_eq!(message, original);
    }

    #[test]
    fn anthropic_empty_text_events_skip_only_empty_deltas_not_blocks() {
        let mut response = native_response();
        response["output"][0]["content"] = json!([{"type": "output_text", "text": ""}]);
        let message = anthropic::to_message(&response, "m").unwrap();
        let events = anthropic::events(&message).unwrap();
        assert_eq!(
            names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
    }

    #[test]
    fn anthropic_events_reject_missing_usage_and_invalid_tool_input_objects() {
        let message = anthropic::to_message(&native_response(), "m").unwrap();
        for (pointer, value) in [
            ("/usage", Value::Null),
            ("/usage/output_tokens", json!(-1)),
            ("/usage/output_tokens", Value::Null),
            (
                "/content",
                json!([{"type": "tool_use", "id": "a", "name": "lookup", "input": []}]),
            ),
            (
                "/content",
                json!([{"type": "tool_use", "id": "a", "name": "lookup", "input": null}]),
            ),
            (
                "/content",
                json!([{"type": "thinking", "thinking": "opaque"}]),
            ),
            ("/stop_reason", Value::Null),
            ("/id", json!("")),
        ] {
            let mut invalid = message.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert_error(anthropic::events(&invalid), 502);
        }
    }
}

#[test]
fn tool_first_streamed_output_round_trips_as_one_assistant_turn() {
    let mut codec = ChatResponseStream::new(
        &json!({"model":"m", "input":"run"}),
        Some("resp_tools_first"),
    )
    .unwrap();
    codec.start().unwrap();
    codec.feed(&json!({"id":"chat_tools","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_first","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null}]})).unwrap();
    codec.feed(&json!({"id":"chat_tools","choices":[{"index":0,"delta":{"content":"Checking."},"finish_reason":"tool_calls"}]})).unwrap();
    codec.finish().unwrap();
    let response = codec.response().unwrap();
    assert_eq!(response["output"][0]["type"], "function_call");
    assert_eq!(response["output"][1]["type"], "message");
    let original = response["output"].clone();
    let mut input = vec![json!({"role":"user","content":"run"})];
    input.extend(original.as_array().unwrap().iter().cloned());
    let missing = json!({"model":"m","input":input});
    assert!(responses_to_chat(&missing, false).is_err());
    input.push(json!({"type":"function_call_output","call_id":"call_first","output":"found"}));
    let next = responses_to_chat(&json!({"model":"m","input":input}), false).unwrap();
    assert_eq!(next["messages"][1]["role"], "assistant");
    assert_eq!(next["messages"][1]["tool_calls"][0]["id"], "call_first");
    assert_eq!(
        next["messages"][1]["content"],
        json!([{"type":"text","text":"Checking."}])
    );
    assert_eq!(next["messages"][2]["tool_call_id"], "call_first");
    assert_eq!(response["output"], original);
    input.insert(2, json!({"role":"user","content":"illegal interleaving"}));
    assert!(responses_to_chat(&json!({"model":"m","input":input}), false).is_err());
}
