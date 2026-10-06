use super::*;
use crate::test_support::{CacheFile, Fixture, Reply, authorization, model, response};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

fn context() -> RequestContext {
    RequestContext::new(Duration::from_secs(5)).unwrap()
}

async fn connect(fixture: &Fixture) -> Arc<Backend> {
    Backend::github(
        Credential::new("user".into(), "fixture-oauth".into()).unwrap(),
        AuthClient::fixture(fixture.origin.clone()).unwrap(),
        &context(),
    )
    .await
    .unwrap()
}

fn fast_model(id: &str) -> Value {
    let mut fast = model(id, true);
    fast["supported_endpoints"] = json!(["/responses"]);
    fast
}

#[test]
fn future_fast_requires_the_exact_enabled_openai_responses_only_variant() {
    let base = model("gpt-future", true);
    let eligible = fast_model("gpt-future-fast");
    let catalog = models::github(json!({"data": [base, eligible]}), "user").unwrap();
    assert_eq!(
        catalog.value["models"][0]["additional_speed_tiers"],
        json!(["fast"])
    );
    assert_eq!(
        models::select(&catalog, "gpt-future", &json!("default"), true)
            .unwrap()
            .id,
        "gpt-future"
    );
    assert_eq!(
        models::select(&catalog, "gpt-future", &json!("priority"), true)
            .unwrap()
            .id,
        "gpt-future-fast"
    );
    assert_eq!(
        models::select(&catalog, "gpt-future-fast", &json!("priority"), true)
            .unwrap()
            .id,
        "gpt-future-fast"
    );
    for patch in [
        json!({"id": "gpt-future-other-fast"}),
        json!({"vendor": "Other"}),
        json!({"policy": {}}),
        json!({"policy": {"state": "disabled"}}),
        json!({"model_picker_enabled": false}),
        json!({"supported_endpoints": ["/chat/completions"]}),
        json!({"supported_endpoints": ["/responses", "/chat/completions"]}),
        json!({"capabilities": {"supports": {"tool_calls": false}}}),
    ] {
        let mut candidate = fast_model("gpt-future-fast");
        candidate
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        let catalog = models::github(
            json!({"data": [model("gpt-future", true), candidate]}),
            "user",
        )
        .unwrap();
        assert_eq!(
            catalog.value["models"][0]["additional_speed_tiers"],
            json!([])
        );
        assert_eq!(
            models::select(&catalog, "gpt-future", &json!("priority"), true)
                .unwrap_err()
                .status,
            400
        );
    }
}

#[tokio::test]
async fn gpt61_catalog_keeps_advertised_efforts_and_dispatches_exact_standard_or_fast() {
    let mut base = model("gpt-6.1-sol", true);
    base["capabilities"]["supports"]["reasoning_effort"] = json!(["low", "medium", "high", "max"]);
    let mut fast = fast_model("gpt-6.1-sol-fast");
    fast["capabilities"]["supports"]["reasoning_effort"] = json!(["high", "max"]);
    let fixture = Fixture::start(move |request, origin| match request.path.as_str() {
        "/user" => Reply::json(json!({"login": "user"})),
        "/copilot_internal/v2/token" => Reply::json(authorization(origin, "fixture-service")),
        "/models" => Reply::json(json!({"data": [base, fast]})),
        "/responses" => Reply::json(response("resp", "done")),
        _ => panic!("Unexpected inference route"),
    })
    .await;
    let backend = connect(&fixture).await;
    let catalog = backend.model_catalog(&context()).await.unwrap();
    let entry = &catalog["models"][0];
    assert_eq!(entry["slug"], "gpt-6.1-sol");
    assert_eq!(entry["default_reasoning_level"], "medium");
    assert_eq!(
        entry["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|level| level["effort"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["low", "medium", "high", "max"]
    );
    for tier in ["default", "priority"] {
        backend.complete_responses(json!({"model":"gpt-6.1-sol","input":"hello","service_tier":tier,"reasoning":{"effort":"max"},"store":false}), context()).await.unwrap();
    }
    let calls = fixture
        .requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].json()["model"], "gpt-6.1-sol");
    assert_eq!(calls[1].json()["model"], "gpt-6.1-sol-fast");
    assert_eq!(
        backend
            .complete_responses(
                json!({"model":"gpt-6.1-sol","input":"hello","reasoning":{"effort":"ultra"}}),
                context()
            )
            .await
            .unwrap_err()
            .status,
        400
    );
    assert_eq!(
        fixture
            .requests()
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        2
    );
}

#[tokio::test]
async fn slow_model_picker_uses_marked_display_cache_without_authorizing_inference() {
    let metadata = Arc::new(AtomicUsize::new(0));
    let calls = metadata.clone();
    let gate = Arc::new(Notify::new());
    let fixture = Fixture::start(move |request, origin| match request.path.as_str() {
        "/user" => Reply::json(json!({"login": "user"})),
        "/copilot_internal/v2/token" => Reply::json(authorization(origin, "fixture-service")),
        "/models" => {
            let mut reply = Reply::json(json!({"data": [model("m", true)]}));
            if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                reply.chunks.insert(0, Vec::new());
                reply.gate = Some(gate.clone());
            }
            reply
        }
        _ => panic!("Display cache must not authorize inference"),
    })
    .await;
    let backend = connect(&fixture).await;
    backend.catalog.lock().await.as_mut().unwrap().expires_at = Instant::now();
    let value = tokio::time::timeout(Duration::from_secs(3), backend.model_catalog(&context()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value["account"], "user");
    assert_eq!(value["catalog_source"], "last_known_good");
    assert_eq!(value["catalog_stale"], true);
    assert!(backend.catalog.lock().await.is_none());
    let short = RequestContext::new(Duration::from_millis(50)).unwrap();
    assert_eq!(
        backend
            .complete_responses(json!({"model":"m","input":"hello"}), short)
            .await
            .unwrap_err()
            .status,
        504
    );
    assert!(
        fixture
            .requests()
            .iter()
            .all(|request| request.method != "POST")
    );
    assert_eq!(metadata.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn mai_catalog_refresh_errors_do_not_use_personal_display_fallback() {
    let metadata = Arc::new(AtomicUsize::new(0));
    let calls = metadata.clone();
    let fixture = Fixture::start(move |request, _| match request.path.as_str() {
        "/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
        "/v1/models" if calls.fetch_add(1, Ordering::SeqCst) == 0 => {
            Reply::json(json!({"data": [{"id": "m"}]}))
        }
        "/v1/models" => Reply::json(json!({"error":{"message":"unavailable"}})).status(503),
        _ => panic!("Stale MAI metadata must not authorize inference"),
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
    assert_eq!(
        backend.model_catalog(&context()).await.unwrap_err().status,
        503
    );
    assert!(backend.catalog.lock().await.is_none());
    assert_eq!(
        backend
            .complete_responses(json!({"model":"m","input":"hello"}), context())
            .await
            .unwrap_err()
            .status,
        503
    );
    assert!(
        fixture
            .requests()
            .iter()
            .all(|request| request.method != "POST")
    );
}

#[tokio::test]
async fn received_auth_errors_retire_display_cache_and_never_become_cached_successes() {
    for status in [401, 403] {
        let metadata = Arc::new(AtomicUsize::new(0));
        let calls = metadata.clone();
        let fixture = Fixture::start(move |request, origin| match request.path.as_str() {
            "/user" => Reply::json(json!({"login": "user"})),
            "/copilot_internal/v2/token" => Reply::json(authorization(origin, "fixture-service")),
            "/models" if calls.fetch_add(1, Ordering::SeqCst) == 0 => {
                Reply::json(json!({"data": [model("m", true)]}))
            }
            "/models" => Reply::json(json!({"error":{"message":"access denied"}})).status(status),
            _ => panic!("Denied account must not infer"),
        })
        .await;
        let backend = connect(&fixture).await;
        backend.catalog.lock().await.as_mut().unwrap().expires_at = Instant::now();
        assert_eq!(
            backend.model_catalog(&context()).await.unwrap_err().status,
            status
        );
        assert!(backend.catalog.lock().await.is_none());
        assert!(backend.display_catalog.read().unwrap().is_none());
        assert!(
            fixture
                .requests()
                .iter()
                .all(|request| request.method != "POST")
        );
    }
}

#[tokio::test]
async fn rejected_metadata_headers_retire_display_cache_without_waiting_for_error_body() {
    for status in [401, 403] {
        for reject_token in [false, true] {
            let exchanges = Arc::new(AtomicUsize::new(0));
            let token_calls = exchanges.clone();
            let metadata = Arc::new(AtomicUsize::new(0));
            let model_calls = metadata.clone();
            let gate = Arc::new(Notify::new());
            let fixture = Fixture::start(move |request, origin| {
                let rejected = || {
                    Reply::gated(
                        Vec::new(),
                        serde_json::to_vec(&json!({"error":{"message":"access denied"}})).unwrap(),
                        gate.clone(),
                    )
                    .status(status)
                };
                match request.path.as_str() {
                    "/user" => Reply::json(json!({"login": "user"})),
                    "/copilot_internal/v2/token" => {
                        if token_calls.fetch_add(1, Ordering::SeqCst) > 0 && reject_token {
                            rejected()
                        } else {
                            Reply::json(authorization(origin, "fixture-service"))
                        }
                    }
                    "/models" => {
                        if model_calls.fetch_add(1, Ordering::SeqCst) > 0 && !reject_token {
                            rejected()
                        } else {
                            Reply::json(json!({"data": [model("m", true)]}))
                        }
                    }
                    _ => panic!("Rejected metadata must not authorize inference"),
                }
            })
            .await;
            let backend = connect(&fixture).await;
            if reject_token {
                let Source::Github(auth) = &backend.source else {
                    panic!("Expected GitHub");
                };
                auth.invalidate(&auth.session(&context()).await.unwrap())
                    .unwrap();
            } else {
                backend.catalog.lock().await.as_mut().unwrap().expires_at = Instant::now();
            }
            assert_eq!(
                backend.model_catalog(&context()).await.unwrap_err().status,
                status
            );
            assert!(backend.display_catalog.read().unwrap().is_none());
            assert_eq!(
                backend
                    .complete_responses(json!({"model":"m","input":"hello"}), context())
                    .await
                    .unwrap_err()
                    .status,
                status
            );
            assert!(
                fixture
                    .requests()
                    .iter()
                    .all(|request| request.method != "POST")
            );
        }
    }
}

#[tokio::test]
async fn changed_endpoint_cannot_use_old_display_catalog_when_new_metadata_fails() {
    let exchanges = Arc::new(AtomicUsize::new(0));
    let calls = exchanges.clone();
    let fixture = Fixture::start(move |request, origin| match request.path.as_str() {
        "/user" => Reply::json(json!({"login": "user"})),
        "/copilot_internal/v2/token" => {
            let prefix = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                "old"
            } else {
                "new"
            };
            let mut value = authorization(origin, "fixture-service");
            value["endpoints"]["api"] = json!(format!("{}{prefix}", origin.as_str()));
            Reply::json(value)
        }
        "/old/models" => Reply::json(json!({"data":[model("m", true)]})),
        "/new/models" => Reply::json(json!({"error":{"message":"unavailable"}})).status(503),
        _ => panic!("Changed endpoint must not infer"),
    })
    .await;
    let backend = connect(&fixture).await;
    let Source::Github(auth) = &backend.source else {
        panic!("Expected GitHub");
    };
    auth.invalidate(&auth.session(&context()).await.unwrap())
        .unwrap();
    assert_eq!(
        backend.model_catalog(&context()).await.unwrap_err().status,
        503
    );
    assert!(backend.display_catalog.read().unwrap().is_none());
    assert!(
        fixture
            .requests()
            .iter()
            .all(|request| request.method != "POST")
    );
}

#[tokio::test]
async fn native_agent_and_custom_tool_continuation_preserve_their_owned_history() {
    let agent = json!({"type":"agent_message","id":"agent_1","author":"reviewer","recipient":"root","content":[{"type":"reasoning_text","text":"Inspect only."}]});
    let expected_agent = agent.clone();
    let inference = Arc::new(AtomicUsize::new(0));
    let calls = inference.clone();
    let fixture = Fixture::start(move |request, origin| match request.path.as_str() {
        "/user" => Reply::json(json!({"login":"user"})),
        "/copilot_internal/v2/token" => Reply::json(authorization(origin,"fixture-service")),
        "/models" => Reply::json(json!({"data":[model("m",true)]})),
        "/responses" => {
            let request = request.json();
            assert_eq!(request["input"][1], expected_agent);
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut result = response("custom_first", "");
                result["output"] = json!([{"type":"custom_tool_call","id":"custom_item","status":"completed","call_id":"call_custom","name":"shell","input":"pwd"}]);
                Reply::json(result)
            } else {
                assert_eq!(request["input"][2]["type"], "custom_tool_call");
                assert_eq!(request["input"][3], json!({"type":"custom_tool_call_output","call_id":"call_custom","output":"C:/project"}));
                Reply::json(response("custom_second", "done"))
            }
        }
        _ => panic!("Unexpected custom-tool route"),
    }).await;
    let backend = connect(&fixture).await;
    backend
        .complete_responses(
            json!({"model":"m","input":[{"role":"user","content":"Inspect."},agent]}),
            context(),
        )
        .await
        .unwrap();
    backend.complete_responses(json!({"model":"m","previous_response_id":"custom_first","input":[{"type":"custom_tool_call_output","call_id":"call_custom","output":"C:/project"}]}), context()).await.unwrap();
    assert_eq!(inference.load(Ordering::SeqCst), 2);
}
