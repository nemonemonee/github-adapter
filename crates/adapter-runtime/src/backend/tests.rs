use super::*;
use crate::selection::Capability;
use crate::test_support::{
    CacheFile, Fixture, Recorded, Reply, authorization, frames, model, response,
};
use futures_util::TryStreamExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

fn context() -> RequestContext {
    RequestContext::new(Duration::from_secs(10)).unwrap()
}

async fn github(
    records: Vec<Value>,
    handler: impl Fn(&Recorded, &Url) -> Reply + Send + Sync + 'static,
) -> (Fixture, Arc<Backend>) {
    let fixture = Fixture::start(move |request, origin| match request.path.as_str() {
        "/user" => Reply::json(json!({"login": "Personal-User"})),
        "/copilot_internal/v2/token" => Reply::json(authorization(origin, "fixture-service")),
        "/models" => Reply::json(json!({"data": records})),
        _ => handler(request, origin),
    })
    .await;
    let backend = Backend::github(
        Credential::new("personal-user".into(), "fixture-oauth".into()).unwrap(),
        AuthClient::fixture(fixture.origin.clone()).unwrap(),
        &context(),
    )
    .await
    .unwrap();
    (fixture, backend)
}

fn inference_requests(fixture: &Fixture) -> Vec<Recorded> {
    fixture
        .requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect()
}

fn native_parts() -> (Vec<Value>, Vec<Value>) {
    let first = vec![
        json!({"type": "response.created", "response": {"id": "resp_first", "status": "in_progress", "output": []}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {
            "id": "msg_first", "type": "message", "role": "assistant", "status": "in_progress", "content": [],
        }}),
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": "msg_delta", "delta": "again"}),
        json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "item_id": "msg_delta", "delta": "again"}),
    ];
    let terminal = vec![
        json!({"type": "response.completed", "response": response("resp_last", "againagain")}),
    ];
    (first, terminal)
}

#[tokio::test]
async fn malformed_preflight_does_not_refresh_even_an_invalidated_credential() {
    let (fixture, backend) = github(vec![model("m", true)], |_, _| {
        panic!("malformed requests must not infer")
    })
    .await;
    let Source::Github(auth) = &backend.source else {
        panic!("expected GitHub");
    };
    auth.invalidate(&auth.session(&context()).await.unwrap())
        .unwrap();
    for changes in [
        json!({"max_output_tokens": 0}),
        json!({"tools": {}}),
        json!({"tools": [null]}),
        json!({"reasoning": false}),
        json!({"previous_response_id": []}),
        json!({"parallel_tool_calls": 1}),
    ] {
        let mut payload = json!({"model": "m", "input": "x"});
        payload
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        assert_eq!(
            backend
                .complete_responses(payload, context())
                .await
                .unwrap_err()
                .status,
            400
        );
    }
    assert_eq!(
            backend.complete_chat(json!({
                "model": "m", "messages": [{"role": "user", "content": "x"}], "max_tokens": false,
            }), context()).await.unwrap_err().status,
            400,
        );
    assert_eq!(fixture.requests().len(), 3);
}

#[tokio::test]
async fn failed_catalog_refresh_cannot_serve_stale_models() {
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let fixture = Fixture::start(move |request, origin| match request.path.as_str() {
        "/user" => Reply::json(json!({"login": "user"})),
        "/copilot_internal/v2/token" => Reply::json(authorization(origin, "fixture-service")),
        "/models" if observed.fetch_add(1, Ordering::SeqCst) == 0 => {
            Reply::json(json!({"data": [model("m", true)]}))
        }
        "/models" => {
            Reply::json(json!({"error": {"message": "revoked fixture-service"}})).status(403)
        }
        _ => panic!("a failed refresh must not infer"),
    })
    .await;
    let backend = Backend::github(
        Credential::new("user".into(), "fixture-oauth".into()).unwrap(),
        AuthClient::fixture(fixture.origin.clone()).unwrap(),
        &context(),
    )
    .await
    .unwrap();
    backend.catalog.lock().await.as_mut().unwrap().expires_at = Instant::now();
    let error = backend.model_catalog(&context()).await.unwrap_err();
    assert_eq!(error.status, 403);
    assert!(!error.message.contains("fixture-service"));
    assert!(backend.catalog.lock().await.is_none());
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert!(inference_requests(&fixture).is_empty());
}

#[tokio::test]
async fn endpoint_refresh_rebinds_catalogs_and_rejects_foreign_endpoint_history() {
    let exchanges = Arc::new(AtomicUsize::new(0));
    let observed = exchanges.clone();
    let fixture = Fixture::start(move |request, origin| match request.path.as_str() {
        "/user" => Reply::json(json!({"login": "user"})),
        "/copilot_internal/v2/token" => {
            let prefix = if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                "first"
            } else {
                "second"
            };
            let mut value = authorization(origin, "fixture-service");
            value["endpoints"]["api"] = json!(format!("{}{prefix}", origin.as_str()));
            Reply::json(value)
        }
        "/first/models" | "/second/models" => Reply::json(json!({"data": [model("m", true)]})),
        "/first/responses" => Reply::json(response("old_endpoint", "done")),
        _ => panic!("history must not be forwarded to a changed endpoint"),
    })
    .await;
    let backend = Backend::github(
        Credential::new("user".into(), "fixture-oauth".into()).unwrap(),
        AuthClient::fixture(fixture.origin.clone()).unwrap(),
        &context(),
    )
    .await
    .unwrap();
    backend
        .complete_responses(json!({"model": "m", "input": "first"}), context())
        .await
        .unwrap();
    let Source::Github(auth) = &backend.source else {
        panic!("expected GitHub");
    };
    auth.invalidate(&auth.session(&context()).await.unwrap())
        .unwrap();
    let error = backend
        .complete_responses(
            json!({"model": "m", "previous_response_id": "old_endpoint", "input": "next"}),
            context(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(exchanges.load(Ordering::SeqCst), 2);
    assert_eq!(
        fixture
            .requests()
            .iter()
            .filter(|request| request.path.ends_with("/models"))
            .count(),
        2
    );
    assert_eq!(inference_requests(&fixture).len(), 1);
    assert!(backend.history.read("old_endpoint").is_ok());
}

#[tokio::test]
async fn terminal_framing_and_output_budgets_are_checked_before_retaining_history() {
    let (_fixture, backend) = github(vec![model("m", true)], |_, _| {
        panic!("this contract is entirely in memory")
    })
    .await;
    let payload = json!({"model": "m", "input": "x"});
    let context = context();
    let prepared = backend
        .prepare(&payload, RequestKind::Responses { stream: true }, &context)
        .await
        .unwrap();
    let mut event = SseEvent::json(
        "response.completed",
        json!({"type": "response.completed", "response": response("budget", "")}),
    );
    let initial = event.to_bytes().unwrap().len();
    event.data.as_mut().unwrap()["response"]["output"][0]["content"][0]["text"] =
        json!("x".repeat(adapter_protocol::sse::MAX_SSE_EVENT_BYTES - initial));
    assert_eq!(
        event.to_bytes().unwrap().len(),
        adapter_protocol::sse::MAX_SSE_EVENT_BYTES
    );
    assert_eq!(
        backend
            .retain_event(
                &payload,
                &prepared,
                &mut event,
                &mut OutputBudget::default(),
                &context
            )
            .unwrap_err()
            .status,
        502
    );
    assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
    let mut event = SseEvent::json(
        "response.completed",
        json!({"type": "response.completed", "response": response("total_budget", "x")}),
    );
    let mut budget = OutputBudget {
        bytes: adapter_protocol::sse::MAX_SSE_STREAM_BYTES,
    };
    assert_eq!(
        backend
            .retain_event(&payload, &prepared, &mut event, &mut budget, &context)
            .unwrap_err()
            .status,
        502
    );
    assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
}

#[tokio::test]
async fn mai_background_controls_remain_native_and_pending_handles_are_not_retained() {
    let fixture = Fixture::start(|request, _| match request.path.as_str() {
        "/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
        "/v1/models" => Reply::json(json!({"data": [{"id": "m"}]})),
        "/v1/responses" => {
            assert_eq!(request.json()["background"], true);
            assert_eq!(request.json()["conversation"], "remote_mai_conversation");
            Reply::json(json!({"id": "pending", "status": "queued", "output": [], "store": true}))
        }
        _ => panic!("unexpected MAI operation"),
    })
    .await;
    let cache = CacheFile::new(&json!({"models": [{"slug": "m"}]}));
    let backend = Backend::connect(
        BackendConfig::Mai {
            upstream: fixture.origin.to_string(),
            models_cache: cache.path.clone(),
        },
        context(),
    )
    .await
    .unwrap();
    let pending = backend.complete_responses(json!({
            "model": "m", "input": "x", "background": true, "conversation": "remote_mai_conversation",
        }), context()).await.unwrap();
    assert_eq!(pending["status"], "queued");
    assert_eq!(pending["store"], true);
    assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
}

#[tokio::test]
async fn readiness_is_metadata_only_and_catalogs_are_account_scoped_and_cached() {
    let (fixture, backend) = github(vec![model("native", true), model("chat", false)], |_, _| {
        panic!("readiness must not infer")
    })
    .await;
    assert_eq!(
        fixture
            .requests()
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        ["/user", "/copilot_internal/v2/token", "/models"]
    );
    let first = backend.model_catalog(&context()).await.unwrap();
    assert_eq!(first, backend.model_catalog(&context()).await.unwrap());
    assert_eq!(fixture.requests().len(), 3);
    assert_eq!(first["provider"], "github");
    assert_eq!(first["account"], "Personal-User");
    assert_eq!(first["models"][0]["shell_type"], "unified_exec");
    assert_eq!(
        first["models"][0]["truncation_policy"],
        json!({"mode": "bytes", "limit": 10_000})
    );
    assert!(first["models"][0]["model_messages"].is_object());
    let health = backend.health();
    assert_eq!(health["application"], "github-adapter");
    assert_eq!(health["engine"], "rust");
    assert_eq!(health["provider"], "github");
    assert_eq!(backend.scope.account, "personal-user");
    assert!(!health.to_string().contains("fixture-oauth"));
    assert!(!format!("{backend:?}").contains("fixture-service"));
}

#[test]
fn capability_states_and_checked_catalog_quotas_are_not_guessed() {
    let mut unknown = model("unknown", true);
    unknown["capabilities"] = json!({"type": "chat"});
    let mut unsupported = model("unsupported", true);
    unsupported["capabilities"]["supports"] =
        json!({"tool_calls": false, "reasoning_effort": false});
    let mut supported = model("supported", true);
    supported["capabilities"]["supports"]["reasoning_effort"] =
        json!(["low", "medium", "high", "max", "medium"]);
    supported["capabilities"]["limits"] = json!({
        "max_context_window_tokens": 1_000_000, "max_prompt_tokens": 872_000, "max_output_tokens": u64::MAX,
    });
    let catalog =
        models::github(json!({"data": [unknown, unsupported, supported]}), "user").unwrap();
    assert!(matches!(catalog.models[0].tools, Capability::Unknown));
    assert!(matches!(catalog.models[0].reasoning, Capability::Unknown));
    assert!(matches!(catalog.models[1].tools, Capability::Unsupported));
    assert!(matches!(
        catalog.models[1].reasoning,
        Capability::Unsupported
    ));
    assert!(catalog.models[1].validate_effort(&json!("high")).is_err());
    assert!(
        catalog.models[0]
            .validate_effort(&json!("future-effort"))
            .is_ok()
    );
    assert_eq!(
        catalog.value["models"][2]["effective_context_window_percent"],
        87
    );
    assert_eq!(
        catalog.value["models"][2]["auto_compact_token_limit"],
        784800
    );
    assert_eq!(
        catalog.value["models"][2]["default_reasoning_level"],
        "medium"
    );
    assert_eq!(
        catalog.value["models"][2]["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    for invalid in [json!(false), json!(-1), json!(1.5), json!("100")] {
        let mut record = model("bad", true);
        record["capabilities"]["limits"]["max_output_tokens"] = invalid;
        assert!(models::github(json!({"data": [record]}), "user").is_err());
    }
}

#[test]
fn disabled_hidden_incompatible_and_duplicate_models_are_not_advertised() {
    let mut hidden = model("hidden", true);
    hidden["model_picker_enabled"] = json!(false);
    let mut disabled = model("disabled", true);
    disabled["policy"]["state"] = json!("disabled");
    let mut embedding = model("embedding", true);
    embedding["capabilities"]["type"] = json!("embeddings");
    let mut wrong_endpoint = model("wrong", true);
    wrong_endpoint["supported_endpoints"] =
        json!(["https://elsewhere.invalid/responses", "/unknown"]);
    let catalog = models::github(
        json!({"data": [hidden, disabled, embedding, wrong_endpoint, model("ok", true)]}),
        "user",
    )
    .unwrap();
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].id, "ok");
    assert_eq!(
        models::github(json!({"data": []}), "user")
            .err()
            .unwrap()
            .status,
        403
    );
    assert!(
        models::github(
            json!({"data": [model("same", true), model("same", true)]}),
            "user"
        )
        .is_err()
    );
}

#[tokio::test]
async fn native_requests_respect_exact_fast_models_caps_and_selected_headers() {
    let mut base = model(models::SOL, true);
    base["capabilities"]["supports"]["reasoning_effort"] = json!(["low", "high", "max"]);
    let mut fast = model(models::SOL_FAST, true);
    fast["supported_endpoints"] = json!(["/responses"]);
    fast["capabilities"]["supports"]["reasoning_effort"] = json!(["low", "high", "max"]);
    let (fixture, backend) = github(vec![base, fast], |request, _| {
        assert_eq!(request.path, "/responses");
        Reply::sse(&[
            json!({"type": "response.completed", "response": response("resp_fast", "done")}),
        ])
    })
    .await;
    let catalog = backend.model_catalog(&context()).await.unwrap();
    assert_eq!(
        catalog["models"][0]["additional_speed_tiers"],
        json!(["fast"])
    );
    let payload = json!({
        "model": models::SOL, "input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "https://example.invalid/image.png"},
        ]}],
        "service_tier": "priority", "store": false, "max_output_tokens": 1500,
        "reasoning": {"effort": "max"}, "headers": {"Authorization": "client-token"},
        "native_extension": {"opaque": true},
    });
    let result = backend
        .complete_responses(payload, context())
        .await
        .unwrap();
    assert_eq!(result["store"], false);
    assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
    let calls = inference_requests(&fixture);
    assert_eq!(calls.len(), 1);
    let request = &calls[0];
    assert_eq!(request.json()["model"], models::SOL_FAST);
    assert_eq!(request.json()["max_output_tokens"], 1000);
    assert_eq!(request.json()["stream"], true);
    assert_eq!(request.headers["accept"], "application/json");
    assert_eq!(request.json()["native_extension"]["opaque"], true);
    assert!(request.json().get("store").is_none());
    assert!(request.json().get("service_tier").is_none());
    assert_eq!(request.headers["authorization"], "Bearer fixture-service");
    assert_eq!(request.headers["x-initiator"], "user");
    assert_eq!(request.headers["copilot-vision-request"], "true");
    for name in [
        "cookie",
        "x-api-key",
        "openai-organization",
        "openai-project",
        "chatgpt-account-id",
    ] {
        assert!(!request.headers.contains_key(name));
    }
}

#[tokio::test]
async fn unavailable_models_fast_tiers_reasoning_and_state_reject_before_inference() {
    let mut record = model("m", true);
    record["capabilities"]["supports"]["reasoning_effort"] = json!(false);
    let (fixture, backend) = github(vec![record], |_, _| {
        panic!("invalid preflight must not infer")
    })
    .await;
    for payload in [
        json!({"model": "gpt-5.6-sol-max", "input": "x"}),
        json!({"model": "m", "input": "x", "service_tier": "priority"}),
        json!({"model": "m", "input": "x", "reasoning": {"effort": "high"}}),
        json!({"model": "m", "input": "x", "store": 0}),
        json!({"model": "m", "input": "x", "background": true}),
        json!({"model": "m", "input": "x", "conversation": "remote"}),
        json!({"model": "m", "input": "x", "tools": [{"type": "image_generation"}], "tool_choice":"required"}),
        json!({"model": "m", "input": "x", "max_output_tokens": true}),
    ] {
        assert_eq!(
            backend
                .complete_responses(payload, context())
                .await
                .unwrap_err()
                .status,
            400
        );
    }
    assert!(inference_requests(&fixture).is_empty());
}

#[tokio::test]
async fn native_streams_are_incremental_normalized_and_retained_before_terminal_delivery() {
    let gate = Arc::new(Notify::new());
    let waiter = gate.clone();
    let (fixture, backend) = github(vec![model("m", true)], move |_, _| {
        let (first, last) = native_parts();
        Reply::gated(frames(&first), frames(&last), waiter.clone())
    })
    .await;
    let mut events = backend
        .clone()
        .stream_responses(json!({"model": "m", "input": "x"}), context())
        .await
        .unwrap();
    let first = events.next().await.unwrap().unwrap();
    assert_eq!(first.kind, "response.created");
    assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
    let mut text = String::new();
    while text != "againagain" {
        let event = events.next().await.unwrap().unwrap();
        if event.kind == "response.output_text.delta" {
            text.push_str(event.data.as_ref().unwrap()["delta"].as_str().unwrap());
        }
    }
    gate.notify_one();
    let mut terminal = None;
    while let Some(event) = events.next().await {
        let event = event.unwrap();
        if is_terminal(&event) {
            let value = terminal_value(&event).unwrap();
            assert_eq!(value["id"], "resp_first");
            assert_eq!(value["output"][0]["id"], "msg_first");
            assert_eq!(value["output"][0]["content"][0]["text"], "againagain");
            assert_eq!(backend.history.read("resp_first").unwrap().response, value);
            terminal = Some(value);
        }
    }
    assert!(terminal.is_some());
    assert_eq!(inference_requests(&fixture).len(), 1);
}

#[tokio::test]
async fn native_history_keeps_ciphertext_calls_and_complete_continuations() {
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let (fixture, backend) = github(vec![model("m", true), model("other", true)], move |_, _| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Reply::json(json!({
                    "id": "resp_tools", "status": "completed", "output": [
                        {"type": "reasoning", "id": "reasoning_1", "encrypted_content": "opaque-ciphertext", "summary": []},
                        {"type": "function_call", "id": "fc_1", "status": "completed", "call_id": "call_1", "name": "lookup", "arguments": "{ \"key\": \"a\\\\b\" }\n"},
                    ],
                }))
            } else { Reply::json(response("resp_child", "done")) }
        }).await;
    backend
        .complete_responses(json!({"model": "m", "input": "look up"}), context())
        .await
        .unwrap();
    backend
        .complete_responses(
            json!({
                "model": "m", "previous_response_id": "resp_tools",
                "input": [{"type": "function_call_output", "call_id": "call_1", "output": "found"}],
            }),
            context(),
        )
        .await
        .unwrap();
    let requests = inference_requests(&fixture);
    let child = requests[1].json();
    assert!(child.get("previous_response_id").is_none());
    assert_eq!(child["input"][1]["encrypted_content"], "opaque-ciphertext");
    assert_eq!(child["input"][2]["arguments"], "{ \"key\": \"a\\\\b\" }\n");
    assert_eq!(child["input"][3]["call_id"], "call_1");
    let error = backend
        .complete_responses(
            json!({"model": "other", "previous_response_id": "resp_tools", "input": "x"}),
            context(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(inference_requests(&fixture).len(), 2);
    backend.history.discard("resp_tools").unwrap();
    assert!(backend.history.read("resp_child").is_ok());
}

#[tokio::test]
async fn chat_bridge_streams_real_fragments_waits_for_done_and_preserves_usage() {
    let gate = Arc::new(Notify::new());
    let waiter = gate.clone();
    let (fixture, backend) = github(vec![model("chat", false)], move |request, _| {
            assert_eq!(request.path, "/chat/completions");
            assert_eq!(request.json()["stream_options"], json!({"include_usage": true}));
            assert_eq!(request.json()["max_tokens"], 100);
            let first = frames(&[
                json!({"id": "chat_same", "choices": [{"index": 0, "delta": {"content": "Checking."}, "finish_reason": null}]}),
                json!({"id": "chat_same", "choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "call_", "function": {"name": "look", "arguments": ""}},
                ]}, "finish_reason": null}]}),
            ]);
            let mut last = frames(&[
                json!({"id": "chat_same", "choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "one", "function": {"name": "up", "arguments": "{\"key\":1}"}},
                ]}, "finish_reason": "tool_calls"}]}),
                json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 6, "reasoning_tokens": 157, "total_tokens": 173}}),
            ]);
            last.extend_from_slice(b"data: [DONE]\n\n");
            Reply::gated(first, last, waiter.clone())
        }).await;
    let mut events = backend
        .clone()
        .stream_responses(
            json!({"model": "chat", "input": "x", "max_output_tokens": 100}),
            context(),
        )
        .await
        .unwrap();
    let mut got_text = false;
    while !got_text {
        let event = events.next().await.unwrap().unwrap();
        got_text = event.kind == "response.output_text.delta";
    }
    assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
    gate.notify_one();
    let remaining = events.try_collect::<Vec<_>>().await.unwrap();
    let terminal = terminal_value(remaining.last().unwrap()).unwrap();
    assert_eq!(terminal["output"][1]["call_id"], "call_one");
    assert_eq!(terminal["output"][1]["name"], "lookup");
    assert_eq!(terminal["output"][1]["arguments"], "{\"key\":1}");
    assert_eq!(terminal["usage"]["output_tokens"], 163);
    assert_eq!(backend.history.statistics().unwrap()["entries"], 1);
    assert_eq!(inference_requests(&fixture).len(), 1);
}

#[tokio::test]
async fn buffered_chat_fallback_and_direct_chat_keep_their_own_protocols() {
    let completion = json!({
        "id": "chat_fixed", "object": "chat.completion",
        "choices": [
            {"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "one"}},
            {"index": 1, "finish_reason": "stop", "message": {"role": "assistant", "audio": {"data": "fixture"}}},
        ],
        "extension": {"preserve": true},
    });
    let original = completion.clone();
    let (fixture, backend) = github(vec![model("chat", false)], move |request, _| {
        assert_eq!(request.path, "/chat/completions");
        assert_eq!(request.headers["authorization"], "Bearer fixture-service");
        assert_eq!(
            request.headers["accept"],
            if request.json()["stream"] == true {
                "text/event-stream"
            } else {
                "application/json"
            }
        );
        let mut value = completion.clone();
        if request.json()["stream"] == true {
            value["choices"].as_array_mut().unwrap().truncate(1);
        }
        Reply::json(value)
    })
    .await;
    let direct = backend
        .complete_chat(
            json!({"model": "chat", "messages": [{"role": "user", "content": "x"}], "n": 2}),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(direct, original);
    assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
    let events = backend
        .clone()
        .stream_responses(
            json!({"model": "chat", "input": "x", "store": false}),
            context(),
        )
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(events.last().unwrap().kind, "response.completed");
    assert_eq!(
        terminal_value(events.last().unwrap()).unwrap()["output_text"],
        "one"
    );
    assert_eq!(inference_requests(&fixture).len(), 2);
}

#[tokio::test]
async fn direct_chat_multi_choice_streams_pass_through_and_require_all_finishes() {
    let chunks = vec![
        json!({"id": "c", "object": "chat.completion.chunk", "choices": [
            {"index": 0, "delta": {"reasoning_content": "direct-only"}, "finish_reason": null},
            {"index": 1, "delta": {"content": "one"}, "finish_reason": "stop"},
        ]}),
        json!({"id": "c", "choices": [{"index": 0, "delta": {"content": "zero"}, "finish_reason": "stop"}]}),
    ];
    let source = chunks.clone();
    let (_fixture, backend) = github(vec![model("chat", false)], move |request, _| {
        assert_eq!(request.path, "/chat/completions");
        assert_eq!(request.headers["accept"], "text/event-stream");
        assert_eq!(request.headers["authorization"], "Bearer fixture-service");
        let mut body = frames(&source);
        body.extend_from_slice(b"data: [DONE]\n\n");
        Reply::raw(200, "text/event-stream", body)
    })
    .await;
    let events = backend
        .clone()
        .stream_chat(
            json!({"model": "chat", "messages": [{"role": "user", "content": "x"}]}),
            context(),
        )
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(events[0].data, Some(chunks[0].clone()));
    assert_eq!(events[1].data, Some(chunks[1].clone()));
    assert!(events[2].done);
    assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
}

#[tokio::test]
async fn compaction_uses_responses_trigger_and_does_not_retain_or_discard_history() {
    let (fixture, backend) = github(vec![model("m", true)], |request, _| {
            assert_eq!(request.path, "/responses");
            assert_eq!(request.headers["accept"], "application/json");
            assert_eq!(request.headers["authorization"], "Bearer fixture-service");
            let input = request.json()["input"].as_array().unwrap().clone();
            assert_eq!(input.iter().filter(|item| item["type"] == "compaction_trigger").count(), 1);
            assert_eq!(input.last().unwrap()["type"], "compaction_trigger");
            assert_eq!(request.json()["stream"], false);
            Reply::json(json!({
                "id": "compact_fixture", "status": "completed",
                "output": [{"type": "compaction", "id": "opaque", "encrypted_content": "genuine-ciphertext"}],
            }))
        }).await;
    let result = backend.compact_responses(json!({"model": "m", "input": [
            {"role": "user", "content": "text"}, {"type": "compaction_trigger"}, {"type": "compaction_trigger"},
        ]}), context()).await.unwrap();
    assert_eq!(result["object"], "response.compaction");
    assert_eq!(
        result["output"][0]["encrypted_content"],
        "genuine-ciphertext"
    );
    assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
    assert_eq!(inference_requests(&fixture).len(), 1);
    assert_eq!(
        backend
            .compact_responses(
                json!({"model": "m", "input": "x", "stream": true}),
                context()
            )
            .await
            .unwrap_err()
            .status,
        400
    );
    assert_eq!(inference_requests(&fixture).len(), 1);
}

#[tokio::test]
async fn malformed_truncated_and_duplicate_terminal_streams_never_retain_success() {
    for (native, bytes) in [
        (
            true,
            frames(&[json!({"type": "response.created", "response": {"id": "r", "output": []}})]),
        ),
        (
            true,
            frames(&[
                json!({"type": "response.completed", "response": response("r", "x")}),
                json!({"type": "response.completed", "response": response("r", "x")}),
            ]),
        ),
        (true, b"data: [DONE]\n\n".to_vec()),
        (
            false,
            frames(&[
                json!({"choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": "stop"}]}),
            ]),
        ),
        (false, b"data: [DONE]\n\n".to_vec()),
        (
            false,
            frames(&[
                json!({"choices": [{"index": 0, "delta": {"fixture-service": "bad"}, "finish_reason": null}]}),
            ]),
        ),
    ] {
        let (_fixture, backend) = github(vec![model("m", native)], move |_, _| {
            Reply::raw(200, "text/event-stream", bytes.clone())
        })
        .await;
        let result = backend
            .clone()
            .stream_responses(json!({"model": "m", "input": "x"}), context())
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await;
        let error = result.unwrap_err();
        assert_eq!(error.status, 502);
        assert!(!error.message.contains("fixture-service"));
        assert!(!error.message.contains("fixture-oauth"));
        assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
    }
}

#[tokio::test]
async fn zstd_native_streams_verify_the_compression_frame_before_retention() {
    let (first, last) = native_parts();
    let mut all = first;
    all.extend(last);
    let encoded = zstd::stream::encode_all(frames(&all).as_slice(), 0).unwrap();
    for truncated in [false, true] {
        let mut bytes = encoded.clone();
        if truncated {
            bytes.pop();
        }
        let (_fixture, backend) = github(vec![model("m", true)], move |_, _| {
            Reply::raw(200, "text/event-stream", bytes.clone()).header("Content-Encoding", "zstd")
        })
        .await;
        let result = backend
            .clone()
            .stream_responses(json!({"model": "m", "input": "x"}), context())
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await;
        if truncated {
            assert_eq!(result.unwrap_err().status, 502);
            assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
        } else {
            assert_eq!(
                terminal_value(result.unwrap().last().unwrap()).unwrap()["id"],
                "resp_first"
            );
            assert_eq!(backend.history.statistics().unwrap()["entries"], 1);
        }
    }
}

#[tokio::test]
async fn dropping_unpolled_or_partial_streams_cancels_owned_transport_and_not_history() {
    for poll_first in [false, true] {
        let gate = Arc::new(Notify::new());
        let (fixture, backend) = github(vec![model("m", true)], move |_, _| {
            let (first, last) = native_parts();
            Reply::gated(frames(&first), frames(&last), gate.clone())
        })
        .await;
        let context = context();
        let cancellation = context.cancellation.clone();
        let mut stream = backend
            .clone()
            .stream_responses(json!({"model": "m", "input": "x"}), context)
            .await
            .unwrap();
        if poll_first {
            assert!(stream.next().await.unwrap().is_ok());
        }
        drop(stream);
        assert!(cancellation.is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.disconnected.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
    }
}

#[tokio::test]
async fn cancellation_and_deadlines_interrupt_buffered_native_reads_without_retry() {
    for cancel in [true, false] {
        let gate = Arc::new(Notify::new());
        let (fixture, backend) = github(vec![model("m", true)], move |_, _| {
            let (first, last) = native_parts();
            Reply::gated(frames(&first), frames(&last), gate.clone())
        })
        .await;
        let ctx = RequestContext::new(if cancel {
            Duration::from_secs(10)
        } else {
            Duration::from_millis(100)
        })
        .unwrap();
        let cancellation = ctx.cancellation.clone();
        let selected = backend.clone();
        let task = tokio::spawn(async move {
            selected
                .complete_responses(json!({"model": "m", "input": "x"}), ctx)
                .await
        });
        while inference_requests(&fixture).is_empty() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        if cancel {
            cancellation.cancel();
        }
        let error = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.status, if cancel { 499 } else { 504 });
        assert_eq!(inference_requests(&fixture).len(), 1);
        assert_eq!(backend.history.statistics().unwrap()["entries"], 0);
    }
}

#[tokio::test]
async fn rejected_service_tokens_invalidate_once_without_replaying_inference() {
    let exchanges = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen_exchanges = exchanges.clone();
    let seen_attempts = attempts.clone();
    let fixture = Fixture::start(move |request, origin| match request.path.as_str() {
        "/user" => Reply::json(json!({"login": "user"})),
        "/copilot_internal/v2/token" => {
            let index = seen_exchanges.fetch_add(1, Ordering::SeqCst);
            Reply::json(authorization(origin, &format!("fixture-service-{index}")))
        }
        "/models" => Reply::json(json!({"data": [model("m", true)]})),
        "/responses" if seen_attempts.fetch_add(1, Ordering::SeqCst) == 0 => {
            Reply::json(json!({"error": {"message": "fixture-service-0 fixture-oauth rejected"}}))
                .status(401)
        }
        "/responses" => Reply::json(response("recovered", "ok")),
        _ => panic!("unexpected path"),
    })
    .await;
    let backend = Backend::github(
        Credential::new("user".into(), "fixture-oauth".into()).unwrap(),
        AuthClient::fixture(fixture.origin.clone()).unwrap(),
        &context(),
    )
    .await
    .unwrap();
    let error = backend
        .complete_responses(json!({"model": "m", "input": "x"}), context())
        .await
        .unwrap_err();
    assert_eq!(error.status, 401);
    assert!(!error.message.contains("fixture-service"));
    assert!(!error.message.contains("fixture-oauth"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(exchanges.load(Ordering::SeqCst), 1);
    backend
        .complete_responses(
            json!({"model": "m", "input": "retry explicitly", "store": false}),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(exchanges.load(Ordering::SeqCst), 2);
    assert_eq!(
        fixture
            .requests()
            .iter()
            .filter(|request| request.path == "/models")
            .count(),
        1
    );
}

#[tokio::test]
async fn wire_body_limits_bound_both_encoded_and_decoded_bytes_and_keep_error_status() {
    let encoded = zstd::stream::encode_all(b"{\"text\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}".as_slice(), 0).unwrap();
    for (status, encoding, bytes, limit) in [
        (200, "identity", b"{}".to_vec(), 2),
        (429, "identity", b"{} ".to_vec(), 2),
        (200, "zstd", encoded, 64),
    ] {
        let fixture = Fixture::start(move |_, _| {
            Reply::raw(status, "application/json", bytes.clone())
                .header("Content-Encoding", encoding)
        })
        .await;
        let client = transport::client(true).unwrap();
        let response = transport::send(client.get(fixture.origin.clone()), &context(), "fixture")
            .await
            .unwrap();
        let result = transport::read_json(response, &context(), limit, "fixture").await;
        if status == 200 && encoding == "identity" {
            assert_eq!(result.unwrap(), json!({}));
        } else {
            assert_eq!(
                result.unwrap_err().status,
                if status == 429 { 429 } else { 502 }
            );
        }
        assert_eq!(fixture.requests().len(), 1);
    }
}

#[tokio::test]
async fn typed_request_kinds_preserve_native_bridge_chat_and_compaction_dispatch() {
    let (fixture, backend) = github(vec![model("native", true), model("chat", false)], |_, _| {
        panic!("preparation must not infer")
    })
    .await;
    for (kind, model, native, stream) in [
        (
            RequestKind::Responses { stream: false },
            "native",
            true,
            true,
        ),
        (
            RequestKind::Responses { stream: true },
            "native",
            true,
            true,
        ),
        (
            RequestKind::Responses { stream: false },
            "chat",
            false,
            false,
        ),
        (RequestKind::Responses { stream: true }, "chat", false, true),
        (RequestKind::Chat { stream: false }, "chat", false, false),
        (RequestKind::Chat { stream: true }, "chat", false, true),
        (RequestKind::Compaction, "native", true, false),
    ] {
        let mut payload = json!({"model": model});
        if kind.is_chat() {
            payload["messages"] = json!([{"role": "user", "content": "hello"}]);
        } else {
            payload["input"] = json!("hello");
        }
        payload["stream"] = json!(kind != RequestKind::Compaction && !kind.stream());
        let original = payload.clone();
        let prepared = backend.prepare(&payload, kind, &context()).await.unwrap();
        assert_eq!(prepared.native, native, "{kind:?}");
        assert_eq!(prepared.outgoing["stream"], stream, "{kind:?}");
        assert_eq!(prepared.outgoing.get("messages").is_some(), !native);
        if kind == RequestKind::Compaction {
            assert_eq!(
                prepared.outgoing["input"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()["type"],
                "compaction_trigger"
            );
        }
        if matches!(kind, RequestKind::Responses { stream: true }) && !native {
            assert_eq!(
                prepared.outgoing["stream_options"],
                json!({"include_usage": true})
            );
        }
        assert_eq!(payload, original);
    }
    assert!(inference_requests(&fixture).is_empty());
    assert_eq!(fixture.requests().len(), 3);
}

#[test]
fn extracted_shape_validation_preserves_error_order_and_mai_controls() {
    let payload = json!({
        "model": "native", "input": "hello", "background": true,
        "conversation": "mai-conversation", "store": true, "stream": true,
    });
    let original = payload.clone();
    assert_eq!(
        request::validate_shape(
            &payload,
            RequestKind::Responses { stream: true },
            Provider::Mai
        )
        .unwrap(),
        "native"
    );
    let github =
        request::validate_shape(&payload, RequestKind::Compaction, Provider::Github).unwrap_err();
    assert_eq!(
        github,
        AdapterError::invalid("Personal mode does not support background Responses.")
    );
    let mai =
        request::validate_shape(&payload, RequestKind::Compaction, Provider::Mai).unwrap_err();
    assert_eq!(
        mai,
        AdapterError::invalid("Compaction returns JSON; stream must be false.")
    );
    let malformed = json!({"model": "native", "input": false, "max_output_tokens": false});
    assert_eq!(
        request::validate_shape(
            &malformed,
            RequestKind::Responses { stream: false },
            Provider::Github
        )
        .unwrap_err(),
        AdapterError::invalid("input must be a string or an array of objects."),
    );
    assert_eq!(payload, original);
}

#[tokio::test]
async fn mai_pending_ids_poll_at_selected_endpoint_and_continue_without_duplicate_history() {
    let fixture =
        Fixture::start(
            |request, _| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/") => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
                ("GET", "/v1/models") => Reply::json(json!({"data": [{"id": "m"}]})),
                ("POST", "/v1/responses") if request.json()["background"] == true => {
                    Reply::json(json!({"id": "pending", "status": "queued", "output": []}))
                }
                ("GET", "/v1/responses/pending") => Reply::json(response("pending", "done")),
                ("POST", "/v1/responses") => {
                    assert_eq!(request.json()["previous_response_id"], "pending");
                    assert_eq!(request.json()["input"], "next");
                    Reply::json(response("child", "next result"))
                }
                ("DELETE", "/v1/responses/pending") => {
                    Reply::json(json!({"id":"pending", "deleted":true}))
                }
                _ => panic!(
                    "unexpected scoped MAI operation {} {}",
                    request.method, request.path
                ),
            },
        )
        .await;
    let cache = CacheFile::new(&json!({"models": [{"slug": "m"}]}));
    let backend = Backend::connect(
        BackendConfig::Mai {
            upstream: fixture.origin.to_string(),
            models_cache: cache.path.clone(),
        },
        context(),
    )
    .await
    .unwrap();
    assert_eq!(
        backend
            .complete_responses(
                json!({"model":"m", "input":"x", "background":true}),
                context()
            )
            .await
            .unwrap()["status"],
        "queued"
    );
    assert_eq!(
        backend
            .response_history("pending", false, context())
            .await
            .unwrap()["status"],
        "completed"
    );
    backend
        .complete_responses(
            json!({"model":"m", "input":"next", "previous_response_id":"pending"}),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(
        backend
            .response_history("pending", true, context())
            .await
            .unwrap()["deleted"],
        true
    );
    let before = fixture.requests().len();
    for id in ["../models", "a/../b", "x\\y", "", ".", "..", "line\nfeed"] {
        assert_eq!(
            backend
                .response_history(id, false, context())
                .await
                .unwrap_err()
                .status,
            400
        );
    }
    assert_eq!(fixture.requests().len(), before);
}

#[tokio::test]
async fn review_alias_is_explicit_account_scoped_and_never_inherits_fast() {
    let (fixture, backend) = github(
        vec![model("gpt-5.5", true), model("custom", true)],
        |request, _| {
            assert!(request.json().get("service_tier").is_none());
            Reply::json(response(
                &format!("review-{}", request.json()["model"].as_str().unwrap()),
                "approved",
            ))
        },
    )
    .await;
    backend
        .complete_responses(
            json!({"model":"codex-auto-review", "input":"review", "service_tier":"priority"}),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(inference_requests(&fixture)[0].json()["model"], "gpt-5.5");
    backend.set_review_model(Some("custom".into())).unwrap();
    backend
        .complete_responses(
            json!({"model":"codex-auto-review", "input":"review", "service_tier":"priority"}),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(inference_requests(&fixture)[1].json()["model"], "custom");
    backend
        .set_review_model(Some("another-account-model".into()))
        .unwrap();
    assert_eq!(
        backend
            .complete_responses(
                json!({"model":"codex-auto-review", "input":"review"}),
                context()
            )
            .await
            .unwrap_err()
            .status,
        400
    );
    assert_eq!(inference_requests(&fixture).len(), 2);
}

#[tokio::test]
async fn implicit_hosted_image_tools_do_not_break_coding_but_forced_generation_is_rejected() {
    let (fixture, backend) = github(vec![model("m", true)], |request, _| {
        assert_eq!(
            request.json()["tools"],
            json!([{"type":"function","name":"image_gen_custom","parameters":{}}])
        );
        Reply::json(response("coding", "done"))
    })
    .await;
    let input = json!({"model":"m","input":"code", "tools":[{"type":"image_generation"},{"type":"function","name":"image_gen_custom","parameters":{}}]});
    backend
        .complete_responses(input.clone(), context())
        .await
        .unwrap();
    for choice in [
        json!({"type":"image_generation"}),
        json!({"type":"image_gen"}),
    ] {
        let mut forced = input.clone();
        forced["tool_choice"] = choice;
        assert_eq!(
            backend
                .complete_responses(forced, context())
                .await
                .unwrap_err()
                .status,
            400
        );
    }
    assert_eq!(backend.complete_responses(json!({"model":"m","input":"image","tools":[{"type":"image_generation"}],"tool_choice":"required"}),context()).await.unwrap_err().status,400);
    assert_eq!(inference_requests(&fixture).len(), 1);
}

fn codex_image_tool() -> Value {
    json!({
        "type": "namespace",
        "name": "image_gen",
        "description": "Tools in the image_gen namespace.",
        "tools": [{
            "type": "function",
            "name": "imagegen",
            "description": "Generate an image.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {"prompt": {"type": "string"}},
                "required": ["prompt"],
                "additionalProperties": false
            }
        }]
    })
}

#[tokio::test]
async fn codex_image_tools_are_filtered_for_native_requests_and_streams() {
    let supported = json!([
        {"type": "function", "name": "image_gen_custom", "parameters": {}},
        {"type": "function", "name": "imagegen", "parameters": {}},
        {"type": "namespace", "name": "image_gen_custom", "tools": [
            {"type": "function", "name": "imagegen", "parameters": {}}
        ]},
        {"type": "namespace", "name": "github_adapter_image", "tools": [
            {"type": "function", "name": "generate_image", "parameters": {}}
        ]},
        {"type": "function", "name": "mcp__github_adapter_image__generate_image", "parameters": {}}
    ]);
    let expected = supported.clone();
    let (fixture, backend) = github(vec![model("m", true)], move |request, _| {
        assert_eq!(request.json()["tools"], expected);
        assert_eq!(request.json()["tool_choice"], "auto");
        Reply::sse(&[json!({
            "type": "response.completed",
            "response": response("coding", "done")
        })])
    })
    .await;
    for image_tool in [
        codex_image_tool(),
        json!({"type": "function", "name": "image_gen.imagegen", "parameters": {}}),
    ] {
        for streaming in [false, true] {
            let mut tools = supported.as_array().unwrap().clone();
            tools.insert(0, image_tool.clone());
            let input = json!({
                "model": "m", "input": "code", "tools": tools, "tool_choice": "auto"
            });
            if streaming {
                let events = backend
                    .clone()
                    .stream_responses(input, context())
                    .await
                    .unwrap()
                    .try_collect::<Vec<_>>()
                    .await
                    .unwrap();
                assert!(
                    events
                        .iter()
                        .any(|event| event.kind == "response.completed")
                );
            } else {
                assert_eq!(
                    backend.complete_responses(input, context()).await.unwrap()["status"],
                    "completed"
                );
            }
        }
    }
    assert_eq!(inference_requests(&fixture).len(), 4);
}

#[tokio::test]
async fn explicit_codex_image_tools_are_rejected_before_dispatch() {
    let (fixture, backend) = github(vec![model("m", true)], |_, _| {
        panic!("explicit unavailable image tools must not infer")
    })
    .await;
    for choice in [
        json!({"type": "namespace", "name": "image_gen"}),
        json!({"type": "function", "name": "image_gen.imagegen"}),
        json!({"type": "function", "namespace": "image_gen", "name": "imagegen"}),
        json!({"type": "function", "function": {"name": "image_gen.imagegen"}}),
        json!({"type": "allowed_tools", "mode": "required", "tools": [
            {"type": "function", "namespace": "image_gen", "name": "imagegen"}
        ]}),
        json!("required"),
    ] {
        let error = backend
            .complete_responses(
                json!({
                    "model": "m", "input": "image",
                    "tools": [codex_image_tool()], "tool_choice": choice
                }),
                context(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.message.contains("image tool"));
    }
    assert!(inference_requests(&fixture).is_empty());
}

#[tokio::test]
async fn codex_image_tools_are_filtered_before_chat_translation() {
    let (fixture, backend) = github(vec![model("chat", false)], |_, _| {
        panic!("request preparation must not infer")
    })
    .await;
    let context = context();
    let supported = json!({"type": "function", "name": "lookup", "parameters": {}});
    let expected = json!([{
        "type": "function", "function": {"name": "lookup", "parameters": {}}
    }]);
    let prepared = backend
        .prepare(
            &json!({
                "model": "chat", "input": "code",
                "tools": [codex_image_tool(), supported], "tool_choice": "required"
            }),
            RequestKind::Responses { stream: false },
            &context,
        )
        .await
        .unwrap();
    assert_eq!(prepared.outgoing["tools"], expected);
    assert_eq!(prepared.outgoing["tool_choice"], "required");
    let prepared = backend
        .prepare(
            &json!({
                "model": "chat", "messages": [{"role": "user", "content": "code"}],
                "tools": [
                    {"type": "function", "function": {"name": "image_gen.imagegen", "parameters": {}}},
                    expected[0]
                ]
            }),
            RequestKind::Chat { stream: false },
            &context,
        )
        .await
        .unwrap();
    assert_eq!(prepared.outgoing["tools"], expected);
    assert!(inference_requests(&fixture).is_empty());
}

fn encrypted_input() -> Value {
    json!({"model":"m","input":[{"type":"reasoning","id":"rs_private","encrypted_content":"ciphertext-must-not-be-logged","summary":[]},{"role":"user","content":"visible input"}]})
}
fn encrypted_failure() -> Value {
    json!({"type":"response.failed","response":{"id":"resp_failed","status":"failed","output":[],"error":{"code":"invalid_encrypted_content","message":"The encrypted content could not be verified and could not be decrypted: ciphertext-must-not-be-logged"}}})
}

#[tokio::test]
async fn opted_in_encrypted_failure_recovery_works_for_unary_and_stream_and_is_private() {
    for streaming in [false, true] {
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let (_fixture,backend)=github(vec![model("m",true)],move |request,_| {
            if calls.fetch_add(1,Ordering::SeqCst)==0 {
                assert_eq!(request.json()["input"][0]["type"],"reasoning");
                Reply::sse(&[json!({"type":"response.created","response":{"id":"resp_failed","status":"in_progress","output":[]}}),encrypted_failure()]).header("x-request-id","req_safe")
            } else {
                assert_eq!(request.json()["input"],json!([{"role":"user","content":"visible input"}]));
                Reply::sse(&[json!({"type":"response.completed","response":response("resp_recovered","done")})])
            }
        }).await;
        backend.set_encrypted_state_recovery(true);
        if streaming {
            let events = backend
                .clone()
                .stream_responses(encrypted_input(), context())
                .await
                .unwrap()
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            assert!(
                events
                    .iter()
                    .any(|event| event.kind == "response.completed")
            );
            assert!(!events.iter().any(|event| {
                event
                    .to_bytes()
                    .unwrap()
                    .windows(11)
                    .any(|x| x == b"resp_failed")
            }));
        } else {
            assert_eq!(
                backend
                    .complete_responses(encrypted_input(), context())
                    .await
                    .unwrap()["status"],
                "completed"
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let failures = backend.health()["response_failures"].clone();
        assert_eq!(failures[0]["recovery"], "succeeded");
        assert_eq!(failures[0]["request_id"], "req_safe");
        assert!(
            !failures
                .to_string()
                .contains("ciphertext-must-not-be-logged")
        );
        assert!(!failures.to_string().contains("visible input"));
    }
}

#[tokio::test]
async fn recovery_never_replays_visible_output_compaction_or_a_second_failure() {
    for kind in ["disabled", "visible", "compaction", "second", "http"] {
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let (_fixture, backend) = github(vec![model("m", true)], move |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            if kind == "http" {
                return Reply::json(encrypted_failure()["response"].clone()).status(400);
            }
            let mut events = if kind == "visible" {
                native_parts().0
            } else {
                vec![]
            };
            events.push(encrypted_failure());
            Reply::sse(&events)
        })
        .await;
        backend.set_encrypted_state_recovery(kind != "disabled");
        let mut payload = encrypted_input();
        if kind == "compaction" {
            payload["input"].as_array_mut().unwrap().insert(
                0,
                json!({"type":"compaction","encrypted_content":"only-surviving-context"}),
            );
        }
        let result = backend.clone().stream_responses(payload, context()).await;
        let failed = match result {
            Err(_) => true,
            Ok(stream) => stream.try_collect::<Vec<_>>().await.is_err(),
        };
        assert!(failed);
        assert_eq!(
            count.load(Ordering::SeqCst),
            if kind == "second" || kind == "http" {
                2
            } else {
                1
            },
            "{kind}"
        );
    }
}

#[tokio::test]
async fn diagnostics_are_bounded_even_with_recovery_disabled() {
    let (_fixture, backend) = github(vec![model("m", true)], |_, _| {
        Reply::sse(&[encrypted_failure()])
    })
    .await;
    for _ in 0..14 {
        assert!(
            backend
                .complete_responses(encrypted_input(), context())
                .await
                .is_err()
        );
    }
    let health = backend.health();
    assert_eq!(health["response_failures"].as_array().unwrap().len(), 10);
    assert_eq!(health["response_failures"][0]["recovery"], "disabled");
}

#[tokio::test]
async fn recovery_respects_prelude_budget_and_never_replays_rate_limits() {
    for oversized in [false, true] {
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let (_fixture,backend)=github(vec![model("m",true)],move |_,_| {
            calls.fetch_add(1,Ordering::SeqCst);
            if !oversized {return Reply::json(encrypted_failure()["response"].clone()).status(429);}
            Reply::sse(&[json!({"type":"response.created","response":{"id":"resp_prelude","status":"in_progress","output":[],"extension":"x".repeat(256*1024)}}),encrypted_failure()])
        }).await;
        backend.set_encrypted_state_recovery(true);
        let result = backend
            .clone()
            .stream_responses(encrypted_input(), context())
            .await;
        assert!(match result {
            Err(_) => true,
            Ok(stream) => stream.try_collect::<Vec<_>>().await.is_err(),
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn encrypted_recovery_never_replays_output_embedded_in_sse_resources() {
    for streaming in [false, true] {
        for kind in [
            "response.created",
            "response.in_progress",
            "response.failed",
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let count = calls.clone();
            let (_fixture, backend) = github(vec![model("m", true)], move |_, _| {
                count.fetch_add(1, Ordering::SeqCst);
                let partial = json!([{"type":"message","id":"msg_partial","role":"assistant","status":"incomplete","content":[{"type":"output_text","text":"partial output","annotations":[]}]}]);
                let mut failure = encrypted_failure();
                let mut events = Vec::new();
                if kind == "response.failed" {
                    events.push(json!({"type":"response.created","response":{"id":"resp_failed","status":"in_progress","output":[]}}));
                    failure["response"]["output"] = partial;
                } else {
                    events.push(json!({"type":kind,"response":{"id":"resp_failed","status":"in_progress","output":partial}}));
                }
                events.push(failure);
                Reply::sse(&events)
            }).await;
            backend.set_encrypted_state_recovery(true);
            let result = if streaming {
                backend
                    .clone()
                    .stream_responses(encrypted_input(), context())
                    .await
                    .unwrap()
                    .try_collect::<Vec<_>>()
                    .await
                    .map(|_| ())
            } else {
                backend
                    .complete_responses(encrypted_input(), context())
                    .await
                    .map(|_| ())
            };
            assert!(
                result.is_err(),
                "The original upstream failure must remain a failure."
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "Observed output must forbid replay: {kind}, streaming={streaming}"
            );
        }
    }
}

#[tokio::test]
async fn encrypted_recovery_never_replays_output_in_non_success_json() {
    for streaming in [false, true] {
        for nested in [false, true] {
            for output in [
                json!([{"type":"message","content":[]}]),
                json!("malformed output"),
            ] {
                let count = Arc::new(AtomicUsize::new(0));
                let calls = count.clone();
                let mut resource = encrypted_failure()["response"].clone();
                resource["output"] = output;
                let failure = if nested {
                    json!({"error":resource["error"],"response":resource})
                } else {
                    resource
                };
                let (_fixture, backend) = github(vec![model("m", true)], move |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Reply::json(failure.clone()).status(400)
                })
                .await;
                backend.set_encrypted_state_recovery(true);
                let failed = if streaming {
                    match backend
                        .clone()
                        .stream_responses(encrypted_input(), context())
                        .await
                    {
                        Err(_) => true,
                        Ok(events) => events.try_collect::<Vec<_>>().await.is_err(),
                    }
                } else {
                    backend
                        .complete_responses(encrypted_input(), context())
                        .await
                        .is_err()
                };
                assert!(failed);
                assert_eq!(
                    count.load(Ordering::SeqCst),
                    1,
                    "streaming={streaming}, nested={nested}"
                );
                assert_eq!(
                    backend.health()["response_failures"][0]["recovery"],
                    "not_attempted"
                );
            }
        }
    }
}

#[tokio::test]
async fn mai_published_opaque_ids_are_addressed_as_single_url_segments() {
    for (id, encoded) in [
        ("opaque+/response==", "opaque+%2Fresponse=="),
        ("literal%2Fid", "literal%252Fid"),
        ("x?api_key=x", "x%3Fapi_key=x"),
    ] {
        let remote = format!("/v1/responses/{encoded}");
        let fixture = Fixture::start(move |request, _| {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/") => Reply::json(json!({"message":"MAI LLM Proxy server is running"})),
                ("GET", "/v1/models") => Reply::json(json!({"data":[{"id":"m"}]})),
                ("POST", "/v1/responses") => {
                    Reply::json(json!({"id":id,"status":"queued","output":[]}))
                }
                ("GET", path) if path == remote => Reply::json(response(id, "done")),
                ("DELETE", path) if path == remote => Reply::json(json!({"id":id,"deleted":true})),
                _ => panic!("Unexpected MAI request {} {}", request.method, request.path),
            }
        })
        .await;
        let cache = CacheFile::new(&json!({"models":[{"slug":"m"}]}));
        let backend = Backend::connect(
            BackendConfig::Mai {
                upstream: fixture.origin.to_string(),
                models_cache: cache.path.clone(),
            },
            context(),
        )
        .await
        .unwrap();
        let queued = backend
            .complete_responses(
                json!({"model":"m","input":"x","background":true}),
                context(),
            )
            .await
            .unwrap();
        assert_eq!(queued["id"], id);
        assert_eq!(
            backend
                .response_history(id, false, context())
                .await
                .unwrap()["status"],
            "completed"
        );
        assert_eq!(
            backend.response_history(id, true, context()).await.unwrap()["deleted"],
            true
        );
    }
}

#[tokio::test]
async fn mai_rejects_idless_pending_and_malformed_retrieved_resources() {
    for (retrieved, resource) in [
        (false, json!({"status":"queued","output":[]})),
        (true, json!({"id":"r","status":"completed","output":null})),
        (
            true,
            json!({"id":"r","status":"completed","output":[],"usage":{"input_tokens":-1}}),
        ),
    ] {
        let fixture = Fixture::start(move |request, _| match request.path.as_str() {
            "/" => Reply::json(json!({"message":"MAI LLM Proxy server is running"})),
            "/v1/models" => Reply::json(json!({"data":[{"id":"m"}]})),
            "/v1/responses" | "/v1/responses/r" => Reply::json(resource.clone()),
            _ => panic!("Unexpected request"),
        })
        .await;
        let cache = CacheFile::new(&json!({"models":[{"slug":"m"}]}));
        let backend = Backend::connect(
            BackendConfig::Mai {
                upstream: fixture.origin.to_string(),
                models_cache: cache.path.clone(),
            },
            context(),
        )
        .await
        .unwrap();
        let result = if retrieved {
            backend.response_history("r", false, context()).await
        } else {
            backend
                .complete_responses(
                    json!({"model":"m","input":"x","background":true}),
                    context(),
                )
                .await
        };
        assert!(result.is_err(), "retrieved={retrieved}");
    }
}

#[tokio::test]
async fn retrieved_mai_outcomes_preserve_valid_failures_and_native_extensions() {
    for status in [
        "queued",
        "in_progress",
        "completed",
        "incomplete",
        "failed",
        "cancelled",
    ] {
        let mut original =
            json!({"id":"r", "status":status, "output":[], "extension":{"opaque":"keep"}});
        if status == "failed" {
            original["error"] =
                json!({"code":"fixture_failure","message":"saved upstream failure"});
        }
        let resource = original.clone();
        let fixture = Fixture::start(move |request, _| match request.path.as_str() {
            "/" => Reply::json(json!({"message":"MAI LLM Proxy server is running"})),
            "/v1/models" => Reply::json(json!({"data":[{"id":"m"}]})),
            "/v1/responses/r" => Reply::json(resource.clone()),
            _ => panic!("Unexpected request"),
        })
        .await;
        let cache = CacheFile::new(&json!({"models":[{"slug":"m"}]}));
        let backend = Backend::connect(
            BackendConfig::Mai {
                upstream: fixture.origin.to_string(),
                models_cache: cache.path.clone(),
            },
            context(),
        )
        .await
        .unwrap();
        assert_eq!(
            backend
                .response_history("r", false, context())
                .await
                .unwrap(),
            original
        );
    }
}

#[tokio::test]
async fn tool_first_streamed_history_can_continue_through_the_selected_bridge() {
    let (fixture, backend) = github(vec![model("chat", false)], |request, _| {
        assert_eq!(request.path, "/chat/completions");
        if request.json()["stream"] == true {
            let mut bytes = frames(&[
                json!({"id":"chat_tools","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_first","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null}]}),
                json!({"id":"chat_tools","choices":[{"index":0,"delta":{"content":"Checking."},"finish_reason":"tool_calls"}]}),
            ]);
            bytes.extend_from_slice(b"data: [DONE]\n\n");
            Reply::raw(200, "text/event-stream", bytes)
        } else {
            let messages = request.json()["messages"].clone();
            assert_eq!(messages[1]["tool_calls"][0]["id"], "call_first");
            assert_eq!(messages[1]["content"], json!([{"type":"text","text":"Checking."}]));
            assert_eq!(messages[2]["tool_call_id"], "call_first");
            Reply::json(json!({"id":"chat_continued","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"continued"}}]}))
        }
    }).await;
    let events = backend
        .clone()
        .stream_responses(json!({"model":"chat","input":"run"}), context())
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let first = terminal_value(events.last().unwrap()).unwrap();
    assert_eq!(first["output"][0]["type"], "function_call");
    assert_eq!(first["output"][1]["type"], "message");
    let next = backend.complete_responses(json!({"model":"chat","previous_response_id":first["id"],"input":[{"type":"function_call_output","call_id":"call_first","output":"found"}]}), context()).await.unwrap();
    assert_eq!(next["output_text"], "continued");
    assert_eq!(inference_requests(&fixture).len(), 2);
}

#[tokio::test]
async fn mai_cannot_publish_handles_that_its_own_polling_contract_rejects() {
    for id in [
        "..".to_owned(),
        "a/../b".into(),
        "back\\slash".into(),
        "line\nfeed".into(),
        "x".repeat(257),
    ] {
        let fixture = Fixture::start(move |request, _| match request.path.as_str() {
            "/" => Reply::json(json!({"message":"MAI LLM Proxy server is running"})),
            "/v1/models" => Reply::json(json!({"data":[{"id":"m"}]})),
            "/v1/responses" => Reply::json(json!({"id":id,"status":"queued","output":[]})),
            _ => panic!("Unexpected request"),
        })
        .await;
        let cache = CacheFile::new(&json!({"models":[{"slug":"m"}]}));
        let backend = Backend::connect(
            BackendConfig::Mai {
                upstream: fixture.origin.to_string(),
                models_cache: cache.path.clone(),
            },
            context(),
        )
        .await
        .unwrap();
        assert_eq!(
            backend
                .complete_responses(
                    json!({"model":"m","input":"x","background":true}),
                    context()
                )
                .await
                .unwrap_err()
                .status,
            502
        );
        assert_eq!(inference_requests(&fixture).len(), 1);
    }
}
