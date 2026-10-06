use std::fs;
use std::path::Path;

use adapter_protocol::compaction;
use adapter_protocol::error::{AdapterError, Result};
use adapter_protocol::identity::NativeStream;
use adapter_protocol::json::{self, MAX_JSON_DEPTH, MAX_NUMBER_BYTES};
use adapter_protocol::sse::{SseDecoder, SseEvent, StreamLimits};
use adapter_protocol::usage::convert_usage;
use serde_json::{Value, json};

fn event(value: Value) -> SseEvent {
    SseEvent::json(
        value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        value,
    )
}

fn native(events: &[Value]) -> Result<Value> {
    let mut stream = NativeStream::new();
    let mut result = Vec::new();
    for value in events {
        result.push(stream.push(event(value.clone()))?.data.expect("JSON event"));
    }
    stream.finish()?;
    Ok(Value::Array(result))
}

fn completed(id: &str, output: Value) -> Value {
    json!({
        "type": "response.completed",
        "response": {"id": id, "status": "completed", "output": output}
    })
}

fn message(id: &str, text: &str) -> Value {
    json!({
        "id": id, "type": "message", "role": "assistant", "phase": "commentary",
        "content": [{"type": "output_text", "text": text, "annotations": []}]
    })
}

fn added(index: usize, item: Value) -> Value {
    json!({"type": "response.output_item.added", "output_index": index, "item": item})
}

fn text_delta(index: usize, id: &str, text: &str) -> Value {
    json!({
        "type": "response.output_text.delta",
        "output_index": index, "content_index": 0, "item_id": id, "delta": text
    })
}

fn assert_status<T: std::fmt::Debug>(result: Result<T>, status: u16) -> AdapterError {
    let error = result.expect_err("expected explicit error");
    assert_eq!(error.status, status);
    error
}

#[test]
fn language_neutral_native_fixtures_include_corrected_cases() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let directory = root.join("conformance").join("fixtures");
    let mut files = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    files.sort();
    let mut owned = 0;
    let mut corrected = 0;
    for path in files {
        let fixture = json::decode(&fs::read(&path).unwrap(), 4 * 1024 * 1024).unwrap();
        let input = &fixture["input"];
        let result = match fixture["operation"].as_str().unwrap() {
            "usage" => convert_usage(&input["usage"]),
            "compaction_request" => compaction::request(&input["request"]),
            "compaction_result" => compaction::response(&input["response"]),
            "native_events" => native(input["events"].as_array().unwrap()),
            _ => continue,
        };
        owned += 1;
        assert_eq!(fixture["version"], 1, "{}", path.display());
        if fixture["classification"] == "correct" {
            corrected += 1;
        } else {
            assert_eq!(fixture["classification"], "preserve", "{}", path.display());
        }
        if let Some(expected) = fixture.get("expected_error") {
            let error = result.unwrap_err();
            assert_eq!(
                u64::from(error.status),
                expected["status"].as_u64().unwrap(),
                "{}",
                path.display()
            );
        } else {
            assert_eq!(result.unwrap(), fixture["expected"], "{}", path.display());
        }
    }
    assert!(owned >= 9, "owned fixtures must not silently disappear");
    assert!(
        corrected >= 2,
        "corrected behavior must not use Python as its oracle"
    );
}

#[test]
fn json_limits_are_explicit_and_numbers_are_not_rounded() {
    let bytes = br#"{"large":123456789012345678901234567890123456789,"fraction":0.12345678901234567890123456789,"future":{"x":[null,true,"NaN"]}}"#;
    let value = json::decode(bytes, bytes.len()).unwrap();
    assert_eq!(
        value["large"].to_string(),
        "123456789012345678901234567890123456789"
    );
    assert_eq!(
        value["fraction"].to_string(),
        "0.12345678901234567890123456789"
    );
    assert_eq!(
        json::decode(&serde_json::to_vec(&value).unwrap(), 1024).unwrap(),
        value
    );
    assert_status(json::decode(bytes, bytes.len() - 1), 400);
    let maximum_number = "9".repeat(MAX_NUMBER_BYTES);
    assert_eq!(
        json::decode(maximum_number.as_bytes(), MAX_NUMBER_BYTES)
            .unwrap()
            .to_string(),
        maximum_number
    );
    assert_status(
        json::decode(
            "9".repeat(MAX_NUMBER_BYTES + 1).as_bytes(),
            MAX_NUMBER_BYTES + 1,
        ),
        400,
    );
    let finite_large_exponent = json::decode(b"1e400", 100).unwrap();
    assert!(finite_large_exponent.is_number());
    assert_eq!(
        json::decode(finite_large_exponent.to_string().as_bytes(), 100).unwrap(),
        finite_large_exponent
    );
    for invalid in [
        &b"NaN"[..],
        b"Infinity",
        b"-Infinity",
        b"{\"x\":NaN}",
        b"{\"x\":1e}",
        b"{\"x\":01}",
        b"[true,]",
        b"{\"x\":\"\xff\"}",
        b"{}{}",
    ] {
        assert_status(json::decode(invalid, 1024), 400);
    }
}

#[test]
fn json_depth_counts_containers_not_braces_or_numbers_in_strings() {
    let at_limit = format!(
        "{}0{}",
        "[".repeat(MAX_JSON_DEPTH),
        "]".repeat(MAX_JSON_DEPTH)
    );
    assert!(json::decode(at_limit.as_bytes(), 1024).is_ok());
    let too_deep = format!("[{at_limit}]");
    assert_status(json::decode(too_deep.as_bytes(), 1024), 400);
    let string =
        json!({"text": format!("\\\"{}{}", "[{".repeat(1000), "1".repeat(MAX_NUMBER_BYTES + 1))});
    let bytes = serde_json::to_vec(&string).unwrap();
    assert_eq!(json::decode(&bytes, bytes.len()).unwrap(), string);
}

#[test]
fn json_extension_keys_are_never_interpreted_as_serializer_tags() {
    for bytes in [
        br#"{"$serde_json::private::Number":"123"}"#.as_slice(),
        br#"{"$serde_json::private::RawValue":"false"}"#,
        br#"{"nested":[{"$serde_json::private::Num\u0062er":"123","other":true}]}"#,
        b" \r\n {\"$serde_json::private::Number\":\"123\"} \r\n",
    ] {
        let value = json::decode(bytes, 1024).unwrap();
        assert!(value.is_object());
        if let Some(items) = value.get("nested").and_then(Value::as_array) {
            assert_eq!(items[0]["$serde_json::private::Number"], "123");
            assert_eq!(items[0]["other"], true);
        } else {
            assert_eq!(value.as_object().unwrap().len(), 1);
            assert!(
                value
                    .as_object()
                    .unwrap()
                    .values()
                    .next()
                    .unwrap()
                    .is_string()
            );
        }
    }
}

#[test]
fn json_preserves_insertion_order_even_with_private_extension_keys() {
    for bytes in [
        br#"{"z":1,"a":{"y":2,"x":1}}"#.as_slice(),
        br#"{"z":1,"$serde_json::private::Number":"123","a":{"y":2,"x":1}}"#,
    ] {
        let value = json::decode(bytes, 1024).unwrap();
        assert_eq!(serde_json::to_vec(&value).unwrap(), bytes);
    }
    let value = json::decode(
        br#"{"z":0,"$serde_json::private::Number":"123","a":1,"z":2}"#,
        1024,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_string(&value).unwrap(),
        r#"{"z":2,"$serde_json::private::Number":"123","a":1}"#
    );
}

fn decode_chunks(bytes: &[u8], chunks: &[usize], limits: StreamLimits) -> Result<Vec<SseEvent>> {
    let mut decoder = SseDecoder::new(limits);
    let mut events = Vec::new();
    let mut position = 0;
    for &size in chunks {
        let end = (position + size).min(bytes.len());
        events.extend(decoder.push(&bytes[position..end])?);
        position = end;
    }
    events.extend(decoder.push(&bytes[position..])?);
    events.extend(decoder.finish()?);
    Ok(events)
}

#[test]
fn sse_every_boundary_preserves_unicode_multiline_data_comments_and_extensions() {
    let bytes = concat!(
        "\u{feff}: keepalive\r\n\r\n",
        "id: ignored-by-this-protocol\r\n",
        "retry: 1000\r\n",
        "event: custom\r\n",
        "data: {\"type\":\"custom\",\"text\":\"雪 ☃\", \r\n",
        "data: \"future\":{\"n\":123456789012345678901234567890}}\r\n\r\n",
        "data: [DONE]\r\n\r\n"
    )
    .as_bytes();
    let expected = decode_chunks(bytes, &[], StreamLimits::default()).unwrap();
    assert_eq!(expected.len(), 3);
    assert_eq!(expected[0].comment.as_deref(), Some("keepalive"));
    assert_eq!(expected[1].kind, "custom");
    assert_eq!(expected[1].data.as_ref().unwrap()["text"], "雪 ☃");
    assert_eq!(
        expected[1].data.as_ref().unwrap()["future"]["n"].to_string(),
        "123456789012345678901234567890"
    );
    assert!(expected[2].done);
    for split in 0..=bytes.len() {
        assert_eq!(
            decode_chunks(bytes, &[split], StreamLimits::default()).unwrap(),
            expected
        );
    }
    assert_eq!(
        decode_chunks(bytes, &vec![1; bytes.len()], StreamLimits::default()).unwrap(),
        expected
    );
}

#[test]
fn sse_cr_lf_eof_and_event_type_precedence_match_the_contract() {
    for ending in ["\n", "\r\n", "\r"] {
        let bytes =
            format!("event: header{ending}data: {{\"type\":\"body\",\"x\":1}}{ending}{ending}");
        let events = decode_chunks(
            bytes.as_bytes(),
            &vec![1; bytes.len()],
            StreamLimits::default(),
        )
        .unwrap();
        assert_eq!(
            events,
            vec![SseEvent::json("body", json!({"type": "body", "x": 1}))]
        );
    }
    let events = decode_chunks(
        b"event: extension\ndata: {\"opaque\":true}",
        &[1, 3, 2],
        StreamLimits::default(),
    )
    .unwrap();
    assert_eq!(
        events,
        vec![SseEvent::json("extension", json!({"opaque": true}))]
    );
    assert!(events[0].data.as_ref().unwrap().get("type").is_none());
    assert!(
        decode_chunks(b"event: unused\n\n\n", &[], StreamLimits::default())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        decode_chunks(b": final comment", &[], StreamLimits::default()).unwrap()[0]
            .comment
            .as_deref(),
        Some("final comment")
    );
}

#[test]
fn sse_encoding_is_bounded_and_cannot_inject_data_from_comment_or_kind() {
    let original = SseEvent::json("custom", json!({"type": "custom", "value": "\r\n雪"}));
    let bytes = original.to_bytes().unwrap();
    assert_eq!(
        decode_chunks(&bytes, &[], StreamLimits::default()).unwrap(),
        vec![original]
    );
    let comment = SseEvent {
        comment: Some("hello\r\ndata: {\"injected\":true}\nevent: injected".into()),
        ..SseEvent::default()
    };
    let parsed = decode_chunks(&comment.to_bytes().unwrap(), &[], StreamLimits::default()).unwrap();
    assert!(
        parsed
            .iter()
            .all(|event| event.comment.is_some() && event.data.is_none())
    );
    assert_status(SseEvent::json("event\ninjected", json!({})).to_bytes(), 502);
    assert_status(
        SseEvent::json("custom", json!({"type": "different"})).to_bytes(),
        502,
    );
    assert_status(SseEvent::json("", json!([])).to_bytes(), 502);
    assert_status(
        SseEvent {
            done: true,
            data: Some(json!({})),
            ..SseEvent::default()
        }
        .to_bytes(),
        502,
    );
}

#[test]
fn sse_limits_are_inclusive_and_independent_of_chunk_boundaries() {
    let bytes = b"data: {}\r\n\r\n";
    for split in 0..=bytes.len() {
        let limits = StreamLimits {
            max_event_bytes: bytes.len(),
            max_stream_bytes: bytes.len(),
            ..StreamLimits::default()
        };
        assert_eq!(decode_chunks(bytes, &[split], limits).unwrap().len(), 1);
        assert_status(
            decode_chunks(
                bytes,
                &[split],
                StreamLimits {
                    max_event_bytes: bytes.len() - 1,
                    ..limits
                },
            ),
            502,
        );
        assert_status(
            decode_chunks(
                bytes,
                &[split],
                StreamLimits {
                    max_stream_bytes: bytes.len() - 1,
                    ..limits
                },
            ),
            502,
        );
    }
    let twice = [bytes.as_slice(), bytes.as_slice()].concat();
    assert_status(
        decode_chunks(
            &twice,
            &[bytes.len()],
            StreamLimits {
                max_event_bytes: bytes.len(),
                max_stream_bytes: twice.len() - 1,
                ..StreamLimits::default()
            },
        ),
        502,
    );
    let defaults = StreamLimits::default();
    assert_eq!(defaults.max_event_bytes, 16 * 1024 * 1024);
    assert_eq!(defaults.max_stream_bytes, 64 * 1024 * 1024);
    assert_eq!(defaults.max_items, 8192);
}

#[test]
fn malformed_sse_has_explicit_sticky_upstream_errors() {
    let large = format!("data: {{\"n\":{}}}\n\n", "9".repeat(MAX_NUMBER_BYTES + 1));
    for body in [
        b"data: invalid\n\n".as_slice(),
        b"data: null\n\n",
        b"data: []\n\n",
        b"data: {\"type\":123}\n\n",
        b"data: {\"type\":\"bad\\nkind\"}\n\n",
        b"data: {\"x\":NaN}\n\n",
        b"data: {\"x\":\"\xff\"}\n\n",
        large.as_bytes(),
    ] {
        let mut decoder = SseDecoder::default();
        let first = assert_status(decoder.push(body), 502);
        assert_eq!(decoder.push(b"data: {}\n\n").unwrap_err(), first);
        assert_eq!(decoder.finish().unwrap_err(), first);
    }
    let mut incomplete_utf8 = SseDecoder::default();
    incomplete_utf8.push(b": \xe9").unwrap();
    assert_status(incomplete_utf8.finish(), 502);
    let mut finished = SseDecoder::default();
    finished.finish().unwrap();
    assert_status(finished.push(b""), 502);
}

#[test]
fn native_pins_all_identity_paths_without_changing_opaque_content() {
    let original = json!({
        "type": "response.output_item.added", "output_index": 0,
        "id": "event-id", "metadata": {"id": "metadata-id", "item_id": "metadata-item"},
        "item": {
            "id": "opaque+/first==", "type": "message", "role": "assistant",
            "phase": "commentary", "content": [], "future": {"id": "unrelated"}
        }
    });
    let last_item = json!({
        "id": "opaque+/last==", "type": "message", "role": "assistant", "phase": "commentary",
        "content": [{"type": "output_text", "text": "Repeat. Repeat.", "annotations": [{"id": "citation"}]}],
        "future": {"id": "unrelated"}
    });
    let input = vec![
        json!({"type": "response.created", "response_id": "outer", "response": {"id": "response-first", "output": []}}),
        original.clone(),
        text_delta(0, "opaque+/delta==", "Repeat. Repeat."),
        json!({"type": "response.output_item.done", "output_index": 0, "item": last_item.clone()}),
        completed("response-last", json!([last_item])),
    ];
    let output = native(&input).unwrap();
    assert_eq!(output[0]["response_id"], "response-first");
    assert_eq!(output[4]["response"]["id"], "response-first");
    assert_eq!(output[1], original);
    assert_eq!(output[2]["item_id"], "opaque+/first==");
    assert_eq!(output[3]["item"]["id"], "opaque+/first==");
    assert_eq!(output[4]["response"]["output"][0], output[3]["item"]);
    assert_eq!(input[1], original);
}

#[test]
fn native_tracks_rotated_aliases_not_just_canonical_ids() {
    let values = [
        added(0, json!({"id": "first", "type": "message", "content": []})),
        text_delta(0, "rotated", "hello"),
        added(
            1,
            json!({"id": "rotated", "type": "message", "content": []}),
        ),
    ];
    let error = assert_status(native(&values), 502);
    assert!(!error.message.contains("rotated"));
    assert!(!error.message.contains("first"));
    assert_status(
        native(&[
            added(0, json!({"id": "first", "type": "message", "content": []})),
            added(1, json!({"id": "second", "type": "message", "content": []})),
            completed(
                "response",
                json!([message("second", ""), message("first", "")]),
            ),
        ]),
        502,
    );
}

#[test]
fn native_terminal_only_idless_compaction_and_future_types_survive() {
    let value = completed(
        "opaque+/response==",
        json!([
            {"type": "compaction", "encrypted_content": "opaque-ciphertext", "future": {"text": "unchanged"}},
            {"id": "future", "type": "future_tool", "custom": {"arbitrary": [true, null, 5]}}
        ]),
    );
    assert_eq!(
        native(std::slice::from_ref(&value)).unwrap(),
        json!([value])
    );
    let empty = completed("empty", json!([]));
    assert_eq!(
        native(std::slice::from_ref(&empty)).unwrap(),
        json!([empty])
    );
    let unknown = json!({"type": "extension", "id": "not-a-response", "item_id": "not-an-output", "future": true});
    let resource = completed("last", json!([]));
    assert_eq!(
        native(&[unknown.clone(), resource.clone()]).unwrap(),
        json!([unknown, resource])
    );
}

#[test]
fn native_optional_lifecycle_events_and_late_identity_are_supported() {
    let output = native(&[
        text_delta(0, "first-seen-in-delta", "Hello"),
        completed(
            "terminal-only-response-id",
            json!([message("later-item", "Hello")]),
        ),
    ])
    .unwrap();
    assert_eq!(
        output[1]["response"]["output"][0]["id"],
        "first-seen-in-delta"
    );
    let phase = json!({
        "id": "phase", "type": "message", "phase": "future-phase",
        "content": [{"type": "output_text", "text": "Hello"}]
    });
    assert!(native(&[completed("response", json!([phase]))]).is_ok());
    assert!(
        native(&[
            text_delta(0, "empty-placeholder", ""),
            completed("terminal", json!([message("final", "Terminal-only text")])),
        ])
        .is_ok()
    );
}

#[test]
fn native_empty_placeholders_do_not_finalize_content() {
    for (prefix, part_index, done_field, pointer, item) in [
        (
            "response.output_text",
            Some("content_index"),
            "text",
            "/content/0/text",
            message("final", "Terminal-only text"),
        ),
        (
            "response.refusal",
            Some("content_index"),
            "refusal",
            "/content/0/refusal",
            json!({"id": "final", "type": "message", "content": [{"type": "refusal", "refusal": "Cannot answer."}]}),
        ),
        (
            "response.reasoning_summary_text",
            Some("summary_index"),
            "text",
            "/summary/0/text",
            json!({"id": "final", "type": "reasoning", "summary": [{"type": "summary_text", "text": "Terminal summary."}]}),
        ),
        (
            "response.function_call_arguments",
            None,
            "arguments",
            "/arguments",
            json!({"id": "final", "type": "function_call", "call_id": "call", "name": "lookup", "arguments": "{}"}),
        ),
    ] {
        let mut delta = json!({
            "type": format!("{prefix}.delta"), "output_index": 0,
            "item_id": "placeholder", "delta": ""
        });
        if let Some(key) = part_index {
            delta[key] = json!(0);
        }
        assert!(
            native(&[delta.clone(), completed("response", json!([item.clone()])),]).is_ok(),
            "{prefix}"
        );

        let mut done = delta.clone();
        done["type"] = json!(format!("{prefix}.done"));
        done.as_object_mut().unwrap().remove("delta");
        done[done_field] = json!("");
        let mut empty_item = item.clone();
        *empty_item.pointer_mut(pointer).unwrap() = json!("");
        assert!(
            native(&[
                delta.clone(),
                done.clone(),
                completed("response", json!([empty_item])),
            ])
            .is_ok(),
            "{prefix}"
        );
        assert_status(
            native(&[
                delta.clone(),
                done.clone(),
                completed("response", json!([item.clone()])),
            ]),
            502,
        );
        assert_status(native(&[delta.clone(), done, delta.clone()]), 502);

        let mut real_delta = delta.clone();
        real_delta["delta"] = item.pointer(pointer).unwrap().clone();
        assert!(
            native(&[
                delta.clone(),
                real_delta.clone(),
                completed("response", json!([item.clone()])),
            ])
            .is_ok(),
            "{prefix}"
        );
        let mut extended_item = item.clone();
        *extended_item.pointer_mut(pointer).unwrap() = json!(format!(
            "{} additional",
            item.pointer(pointer).unwrap().as_str().unwrap()
        ));
        assert_status(
            native(&[
                delta,
                real_delta,
                completed("response", json!([extended_item])),
            ]),
            502,
        );
    }
}

#[test]
fn native_partial_tools_reasoning_and_usage_are_preserved() {
    let item = json!({
        "id": "call", "type": "function_call", "call_id": "stable-call",
        "name": "lookup", "arguments": "{\"key\":", "status": "incomplete"
    });
    let response = json!({
        "type": "response.incomplete",
        "response": {
            "id": "response", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [
                {"id": "reasoning", "type": "reasoning", "summary": [], "encrypted_content": "opaque+/cipher=="},
                item
            ],
            "usage": {"input_tokens": 9, "output_tokens": 8, "future": {"tokens": 7}},
            "future": {"not_dropped": true}
        }
    });
    assert_eq!(
        native(std::slice::from_ref(&response)).unwrap(),
        json!([response])
    );
}

#[test]
fn native_interleaved_reasoning_summary_refusal_and_late_tool_fields() {
    let reasoning = json!({
        "id": "reasoning-final", "type": "reasoning",
        "summary": [{"type": "summary_text", "text": "Step one."}],
        "encrypted_content": "opaque+/cipher=="
    });
    let refusal = json!({
        "id": "refusal-final", "type": "message", "role": "assistant",
        "content": [{"type": "refusal", "refusal": "Cannot answer."}]
    });
    let call = json!({
        "id": "call-final", "type": "function_call", "call_id": "call-id",
        "name": "lookup", "arguments": "{\"id\":42}"
    });
    let result = native(&[
        json!({
            "type": "response.function_call_arguments.delta", "output_index": 2,
            "item_id": "call-first", "delta": "{\"id\":"
        }),
        json!({
            "type": "response.reasoning_summary_text.delta", "output_index": 0,
            "summary_index": 0, "item_id": "reasoning-first", "delta": "Step "
        }),
        json!({
            "type": "response.refusal.delta", "output_index": 1,
            "content_index": 0, "item_id": "refusal-first", "delta": "Cannot "
        }),
        json!({
            "type": "response.function_call_arguments.delta", "output_index": 2,
            "item_id": "call-rotated", "call_id": "call-id", "name": "lookup", "delta": "42}"
        }),
        json!({
            "type": "response.reasoning_summary_text.delta", "output_index": 0,
            "summary_index": 0, "item_id": "reasoning-rotated", "delta": "one."
        }),
        json!({
            "type": "response.refusal.delta", "output_index": 1,
            "content_index": 0, "item_id": "refusal-rotated", "delta": "answer."
        }),
        completed("response", json!([reasoning, refusal, call])),
    ])
    .unwrap();
    let output = &result[6]["response"]["output"];
    assert_eq!(output[0]["id"], "reasoning-first");
    assert_eq!(output[0]["encrypted_content"], "opaque+/cipher==");
    assert_eq!(output[1]["id"], "refusal-first");
    assert_eq!(output[2]["id"], "call-first");
    assert_eq!(output[2]["call_id"], "call-id");
    assert_eq!(output[2]["arguments"], "{\"id\":42}");
}

#[test]
fn native_finalized_content_cannot_grow_by_reannouncement_or_terminal_additions() {
    let done = json!({
        "type": "response.output_item.done", "output_index": 0,
        "item": message("item", "Hello")
    });
    let mut changed = message("terminal-item", "Hello");
    changed["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "output_text", "text": "extra"}));
    assert_status(
        native(&[done.clone(), completed("response", json!([changed]))]),
        502,
    );
    assert_status(
        native(&[
            done,
            json!({
                "type": "response.content_part.added", "output_index": 0, "content_index": 0,
                "item_id": "changed", "part": {"type": "output_text", "text": ""}
            }),
        ]),
        502,
    );
}

#[test]
fn native_known_fields_types_and_phases_are_checked() {
    let valid = text_delta(0, "id", "hello");
    for (field, invalid) in [
        ("output_index", json!(-1)),
        ("output_index", json!(true)),
        ("output_index", json!("0")),
        ("content_index", json!(-1)),
        ("content_index", json!(0.0)),
        ("delta", json!(123)),
        ("item_id", json!(null)),
        ("item_id", json!("")),
        ("sequence_number", json!(-1)),
        ("sequence_number", json!(true)),
        ("phase", json!(false)),
    ] {
        let mut changed = valid.clone();
        changed[field] = invalid;
        assert_status(native(&[changed]), 502);
    }
    for field in ["output_index", "content_index", "item_id", "delta"] {
        let mut changed = valid.clone();
        changed.as_object_mut().unwrap().remove(field);
        assert_status(native(&[changed]), 502);
    }
    for phase in [json!(false), json!(42), json!("")] {
        let mut item = message("id", "");
        item["phase"] = phase;
        assert_status(native(&[added(0, item)]), 502);
    }
    let mut changed = message("final", "");
    changed["phase"] = json!("final_answer");
    assert_status(
        native(&[
            added(0, message("first", "")),
            completed("response", json!([changed])),
        ]),
        502,
    );
}

#[test]
fn native_rejects_disappeared_items_parts_and_conflicting_final_content() {
    assert_status(
        native(&[
            added(0, json!({"id": "first", "type": "message", "content": []})),
            completed("response", json!([])),
        ]),
        502,
    );
    assert_status(
        native(&[
            text_delta(0, "first", "visible"),
            completed("response", json!([message("last", "different")])),
        ]),
        502,
    );
    let mut second_part = text_delta(0, "first", "visible");
    second_part["content_index"] = json!(1);
    assert_status(
        native(&[
            second_part,
            completed("response", json!([message("last", "visible")])),
        ]),
        502,
    );
    assert_status(
        native(&[
            text_delta(0, "first", "visible"),
            completed(
                "response",
                json!([{"id": "last", "type": "message", "content": []}]),
            ),
        ]),
        502,
    );
    let mut in_progress = message("last", "visible");
    in_progress["status"] = json!("in_progress");
    assert_status(
        native(&[
            text_delta(0, "first", "visible"),
            completed("response", json!([in_progress])),
        ]),
        502,
    );
}

#[test]
fn native_function_id_name_arguments_and_item_type_cannot_change() {
    let first = json!({
        "id": "first", "type": "function_call", "call_id": "call",
        "name": "lookup", "arguments": ""
    });
    for (field, value) in [
        ("call_id", json!("different-call")),
        ("name", json!("different-name")),
        ("type", json!("message")),
    ] {
        let mut last = first.clone();
        last[field] = value;
        assert_status(
            native(&[
                added(0, first.clone()),
                completed("response", json!([last])),
            ]),
            502,
        );
    }
    let mut changed_arguments = first.clone();
    changed_arguments["arguments"] = json!("{\"x\":2}");
    assert_status(
        native(&[
            added(0, first.clone()),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "rotated", "delta": "{\"x\":1}"}),
            completed("response", json!([changed_arguments])),
        ]),
        502,
    );
    let mut duplicate = first.clone();
    duplicate["id"] = json!("other");
    assert_status(native(&[added(0, first), added(1, duplicate)]), 502);
}

#[test]
fn native_done_events_cannot_be_followed_by_more_deltas_or_terminals() {
    let first = text_delta(0, "id", "Hello");
    let done = json!({
        "type": "response.output_text.done", "output_index": 0,
        "content_index": 0, "item_id": "rotated", "text": "Hello"
    });
    assert_status(
        native(&[first.clone(), done.clone(), text_delta(0, "id", "!")]),
        502,
    );
    assert!(
        native(&[
            first,
            done,
            completed("response", json!([message("final", "Hello")]))
        ])
        .is_ok()
    );
    let terminal = completed("response", json!([]));
    assert_status(native(&[terminal.clone(), terminal]), 502);
    let mut stream = NativeStream::new();
    stream
        .push(event(completed("response", json!([]))))
        .unwrap();
    stream
        .push(SseEvent {
            done: true,
            ..SseEvent::default()
        })
        .unwrap();
    stream.finish().unwrap();
    assert_status(stream.push(event(completed("again", json!([])))), 502);
    assert_status(
        NativeStream::new().push(SseEvent {
            done: true,
            ..SseEvent::default()
        }),
        502,
    );
    assert_status(NativeStream::new().finish(), 502);
}

#[test]
fn native_failures_take_precedence_over_invalid_response_identifiers() {
    for failure in [
        json!({"type": "response.failed", "response": {"id": null, "error": {"message": "upstream refused"}}}),
        json!({"type": "error", "item_id": "not-an-output-reference", "error": {"message": "upstream refused"}}),
        json!({"type": "error", "message": "upstream refused"}),
    ] {
        let mut stream = NativeStream::new();
        let error = assert_status(stream.push(event(failure)), 502);
        assert_eq!(error.message, "upstream refused");
        assert_eq!(stream.finish().unwrap_err(), error);
        assert_eq!(
            stream
                .push(event(completed("response", json!([]))))
                .unwrap_err(),
            error
        );
    }
}

#[test]
fn native_default_and_custom_budgets_cover_input_output_and_indexes() {
    let limits = StreamLimits {
        max_items: 1,
        ..StreamLimits::default()
    };
    let mut stream = NativeStream::with_limits(limits);
    assert_status(stream.push(event(added(1, message("id", "")))), 502);
    let mut stream = NativeStream::with_limits(limits);
    assert_status(
        stream.push(event(completed(
            "response",
            json!([message("first", ""), message("second", "")]),
        ))),
        502,
    );
    let mut stream = NativeStream::with_limits(StreamLimits {
        max_stream_bytes: 20,
        ..StreamLimits::default()
    });
    let comment = SseEvent {
        comment: Some("keepalive".into()),
        ..SseEvent::default()
    };
    stream.push(comment.clone()).unwrap();
    assert_status(stream.push(comment), 502);
    let mut stream = NativeStream::with_limits(StreamLimits {
        max_event_bytes: 5,
        ..StreamLimits::default()
    });
    assert_status(stream.push(event(completed("response", json!([])))), 502);
}

#[test]
fn usage_preserves_unknown_null_and_exact_reasoning_accounting() {
    assert_eq!(convert_usage(&Value::Null).unwrap(), Value::Null);
    let negative_zero =
        json::decode(br#"{"prompt_tokens":-0,"completion_tokens":0}"#, 1024).unwrap();
    assert_eq!(
        convert_usage(&negative_zero).unwrap(),
        json!({
            "input_tokens": 0, "output_tokens": 0, "total_tokens": 0
        })
    );
    assert_eq!(
        convert_usage(&json!({
            "prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 173,
            "reasoning_tokens": 157, "prompt_tokens_details": {"cached_tokens": 0}
        }))
        .unwrap(),
        json!({
            "input_tokens": 10, "output_tokens": 163, "total_tokens": 173,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 157}
        })
    );
    assert_eq!(
        convert_usage(&json!({
            "prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14,
            "reasoning_tokens": 2, "prompt_tokens_details": {"cached_tokens": 6, "audio_tokens": 0},
            "completion_tokens_details": {"reasoning_tokens": 2, "accepted_prediction_tokens": 0},
            "future": {"ignored_by_translation": true}
        }))
        .unwrap(),
        json!({
            "input_tokens": 10, "output_tokens": 4, "total_tokens": 14,
            "input_tokens_details": {"cached_tokens": 6},
            "output_tokens_details": {"reasoning_tokens": 2}
        })
    );
    assert_eq!(
        convert_usage(&json!({
            "prompt_tokens": 0, "completion_tokens": 0,
            "prompt_tokens_details": {"cached_tokens": null}, "completion_tokens_details": null
        }))
        .unwrap(),
        json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0})
    );
    assert_eq!(
        convert_usage(&json!({
            "prompt_tokens": u64::MAX, "completion_tokens": 0
        }))
        .unwrap()["total_tokens"],
        json!(u64::MAX)
    );
}

#[test]
fn invalid_or_ambiguous_usage_is_not_fabricated_or_overflowed() {
    for value in [
        json!({}),
        json!([]),
        json!({"prompt_tokens": 1}),
        json!({"prompt_tokens": 10, "completion_tokens": 6, "reasoning_tokens": 2}),
        json!({"prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 30, "reasoning_tokens": 2}),
        json!({"prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 16, "reasoning_tokens": 7}),
        json!({"prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 16,
            "reasoning_tokens": 2, "completion_tokens_details": {"reasoning_tokens": 3}}),
        json!({"prompt_tokens": u64::MAX, "completion_tokens": 1}),
        json!({"prompt_tokens": 1, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 2}}),
        json!({"prompt_tokens": 1, "completion_tokens": 1, "completion_tokens_details": {"unknown": "1"}}),
    ] {
        assert_status(convert_usage(&value), 400);
    }
    for key in [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "reasoning_tokens",
    ] {
        for value in [json!(-1), json!(true), json!(1.0), json!("1")] {
            let mut usage =
                json!({"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14});
            usage[key] = value;
            assert_status(convert_usage(&usage), 400);
        }
    }
}

#[test]
fn compaction_only_adds_a_trigger_and_preserves_genuine_opaque_output() {
    let original = json!({
        "model": "fixture", "stream": null, "future": {"retained": true},
        "input": [
            {"type": "compaction_trigger"},
            {"type": "reasoning", "encrypted_content": "opaque"},
            {"type": "compaction_trigger"},
            {"role": "user", "content": "remember"}
        ]
    });
    let output = compaction::request(&original).unwrap();
    assert_eq!(
        output["input"],
        json!([
            {"type": "reasoning", "encrypted_content": "opaque"},
            {"role": "user", "content": "remember"},
            {"type": "compaction_trigger"}
        ])
    );
    assert_eq!(output["future"], original["future"]);
    assert_eq!(output["stream"], false);
    assert!(original["stream"].is_null());
    let response = json!({
        "id": "opaque-response", "output": [{"type": "compaction", "encrypted_content": "opaque-cipher"}],
        "future": {"id": "unchanged"}
    });
    let mut expected = response.clone();
    expected["object"] = json!("response.compaction");
    assert_eq!(compaction::response(&response).unwrap(), expected);
    assert_eq!(
        compaction::request(&json!({"input": ""})).unwrap()["input"][0],
        json!({"role": "user", "content": ""})
    );
}

#[test]
fn compaction_does_not_accept_empty_input_streaming_or_fabricated_ciphertext() {
    for request in [
        json!({}),
        json!({"input": []}),
        json!({"input": ["bad"]}),
        json!({"input": [{"type": "compaction_trigger"}]}),
        json!({"input": "hello", "stream": true}),
        json!({"input": "hello", "stream": 0}),
    ] {
        assert_status(compaction::request(&request), 400);
    }
    for response in [
        json!({}),
        json!({"id": null, "output": []}),
        json!({"id": "response", "output": [{"type": "compaction"}]}),
        json!({"id": "response", "output": [{"type": "compaction", "encrypted_content": " "}]}),
        json!({"id": "response", "status": "incomplete", "output": [{"type": "compaction", "encrypted_content": "opaque"}]}),
        json!({"id": "response", "output": [{"type": "message", "content": "a summary is not encrypted state"}]}),
        json!({"id": "response", "output": [
            {"type": "compaction", "encrypted_content": "opaque"},
            {"type": "compaction", "encrypted_content": null}
        ]}),
    ] {
        assert_status(compaction::response(&response), 502);
    }
}

fn next_random(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

#[test]
fn ten_thousand_generated_chunk_splits_and_identity_interleavings() {
    for seed in 0..10_000u64 {
        let mut random = seed.wrapping_add(1);
        let mut order = [0usize, 1, 2];
        for slot in (1..order.len()).rev() {
            let other = (next_random(&mut random) as usize) % (slot + 1);
            order.swap(slot, other);
        }
        let canonical = (0..3)
            .map(|index| format!("opaque+/{seed}-{index}=="))
            .collect::<Vec<_>>();
        let final_output = json!([
            message("terminal-message", "雪 repeat repeat"),
            {"id": "terminal-first-tool", "type": "function_call", "call_id": "call-one", "name": "lookup", "arguments": "{\"x\":1}"},
            {"id": "terminal-second-tool", "type": "function_call", "call_id": "call-two", "name": "search", "arguments": "{\"x\":2}"}
        ]);
        let mut values = vec![json!({
            "type": "response.created",
            "response": {"id": format!("response-first-{seed}"), "output": []}
        })];
        for &index in &order {
            let mut item = final_output[index].clone();
            item["id"] = json!(canonical[index]);
            if index == 0 {
                item["content"] = json!([]);
            } else {
                item["arguments"] = json!("");
            }
            values.push(added(index, item));
        }
        for &index in order.iter().rev() {
            let id = format!("rotated-{seed}-{index}");
            values.push(if index == 0 {
                text_delta(index, &id, "雪 repeat repeat")
            } else {
                json!({
                    "type": "response.function_call_arguments.delta", "output_index": index,
                    "item_id": id, "delta": final_output[index]["arguments"]
                })
            });
        }
        if seed % 2 == 0 {
            for &index in &order {
                let mut item = final_output[index].clone();
                item["id"] = json!(format!("done-{seed}-{index}"));
                values.push(json!({"type": "response.output_item.done", "output_index": index, "item": item}));
            }
        }
        let mut terminal = completed("response-rotated", final_output);
        terminal["response"]["extension"] = json!({"seed": seed, "id": "must-not-be-rewritten"});
        values.push(terminal);
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend(event(value).to_bytes().unwrap());
        }
        if seed % 3 == 0 {
            bytes.extend_from_slice(b"data: [DONE]\n\n");
        }
        let mut decoder = SseDecoder::default();
        let mut stream = NativeStream::new();
        let mut position = 0;
        let mut terminal = None;
        while position < bytes.len() {
            let end = (position + 1 + (next_random(&mut random) as usize % 37)).min(bytes.len());
            for frame in decoder.push(&bytes[position..end]).unwrap() {
                let normalized = stream.push(frame).unwrap();
                if normalized.kind == "response.completed" {
                    terminal = normalized.data;
                }
            }
            position = end;
        }
        for frame in decoder.finish().unwrap() {
            let normalized = stream.push(frame).unwrap();
            if normalized.kind == "response.completed" {
                terminal = normalized.data;
            }
        }
        stream.finish().unwrap();
        let terminal = terminal.unwrap();
        assert_eq!(terminal["response"]["id"], format!("response-first-{seed}"));
        assert_eq!(
            terminal["response"]["extension"],
            json!({"seed": seed, "id": "must-not-be-rewritten"})
        );
        for (index, id) in canonical.iter().enumerate() {
            assert_eq!(terminal["response"]["output"][index]["id"], *id);
        }
        assert_eq!(terminal["response"]["output"][1]["call_id"], "call-one");
        assert_eq!(terminal["response"]["output"][2]["call_id"], "call-two");
    }
}
