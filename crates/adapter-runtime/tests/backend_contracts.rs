use adapter_runtime::backend::{Backend, BackendConfig};
use adapter_runtime::context::RequestContext;
use adapter_runtime::selection::Provider;
use futures_util::TryStreamExt;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[allow(dead_code)]
#[path = "../src/test_support.rs"]
mod support;

use support::{CacheFile, Fixture, Reply, response};

fn context() -> RequestContext {
    RequestContext::new(Duration::from_secs(10)).unwrap()
}

fn cache() -> CacheFile {
    CacheFile::new(&json!({"models": [
        {
            "slug": "gpt-5.6-sol", "display_name": "Sol", "default_reasoning_level": "ultra",
            "supported_reasoning_levels": [{"effort": "low", "description": "Low"}, {"effort": "ultra", "description": "Unavailable"}],
            "additional_speed_tiers": ["fast"], "service_tiers": [{"id": "priority"}],
            "model_messages": {"instructions_template": "preserve instructions", "instructions_variables": null},
            "context_window": 32000, "native_extension": {"preserve": true},
        },
        {"slug": "gpt-5.6-luna"},
        {"slug": "not-available"},
    ]}))
}

async fn connect(fixture: &Fixture, cache: &CacheFile, prefix: &str) -> Arc<Backend> {
    Backend::connect(
        BackendConfig::Mai {
            upstream: format!("{}{prefix}", fixture.origin.as_str().trim_end_matches('/')),
            models_cache: cache.path.clone(),
        },
        context(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn messages_use_backend_alias_policy_without_retaining_translated_history() {
    let fixture = Fixture::start(|request, count| match request.path.as_str() {
        "/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
        "/v1/models" => {
            Reply::json(json!({"data": [{"id": "gpt-5.6-sol"}, {"id": "gpt-5.6-luna"}]}))
        }
        "/v1/responses" => Reply::json(response(&format!("message-{count}"), "done")),
        _ => panic!("Unexpected Messages upstream"),
    })
    .await;
    let cache = cache();
    let backend = connect(&fixture, &cache, "").await;
    for (requested, actual, effort) in [
        ("gpt-5.6-sol", "gpt-5.6-sol", "high"),
        ("gpt-5.6-sol-max", "gpt-5.6-sol", "max"),
        ("gpt-5.6-luna-max", "gpt-5.6-luna", "max"),
    ] {
        let message = backend
            .complete_messages(
                json!({
                    "model": requested, "messages": [{"role": "user", "content": "hello"}],
                    "max_tokens": 150_000, "output_config": {"effort": "high"},
                }),
                context(),
            )
            .await
            .unwrap();
        assert_eq!(message["model"], requested);
        assert_eq!(message["content"][0]["text"], "done");
        let sent = fixture.requests().last().unwrap().json();
        assert_eq!(sent["model"], actual);
        assert_eq!(sent["reasoning"]["effort"], effort);
        assert_eq!(sent["max_output_tokens"], 128_000);
        assert_eq!(sent["store"], false);
    }
    assert_eq!(backend.history().statistics().unwrap()["entries"], 0);
}

#[tokio::test]
async fn mai_readiness_catalog_intersection_and_health_are_metadata_only() {
    let fixture = Fixture::start(|request, _| match request.path.as_str() {
        "/proxy/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
        "/proxy/v1/models" => Reply::json(
            json!({"data": [{"id": "gpt-5.6-sol"}, {"id": "gpt-5.6-luna"}, {"id": "unranked"}]}),
        ),
        _ => panic!("readiness must not perform inference"),
    })
    .await;
    let cache = cache();
    let before = std::fs::read(&cache.path).unwrap();
    let backend = connect(&fixture, &cache, "/proxy").await;
    assert_eq!(backend.scope().provider, Provider::Mai);
    assert!(backend.scope().account.ends_with("/proxy"));
    assert_eq!(fixture.requests().len(), 2);
    let catalog = backend.model_catalog(&context()).await.unwrap();
    assert_eq!(catalog["models"].as_array().unwrap().len(), 2);
    assert_eq!(catalog["data"].as_array().unwrap().len(), 2);
    assert_eq!(catalog["data"][0]["id"], "gpt-5.6-sol");
    assert_eq!(catalog["object"], "list");
    assert_eq!(catalog["provider"], "mai");
    assert!(catalog["account"].is_null());
    assert_eq!(
        catalog["models"][0]["supported_reasoning_levels"][1]["effort"],
        "max"
    );
    assert_eq!(catalog["models"][0]["default_reasoning_level"], "xhigh");
    assert_eq!(catalog["models"][0]["additional_speed_tiers"], json!([]));
    assert_eq!(catalog["models"][0]["service_tiers"], json!([]));
    assert_eq!(catalog["models"][0]["native_extension"]["preserve"], true);
    assert_eq!(
        catalog["models"][0]["model_messages"]["instructions_template"],
        "preserve instructions"
    );
    assert_eq!(std::fs::read(&cache.path).unwrap(), before);
    let health = backend.health();
    assert_eq!(health["application"], "github-adapter");
    assert_eq!(health["engine"], "rust");
    assert_eq!(health["provider"], "mai");
    assert!(health["account"].is_null());
    assert!(
        fixture
            .requests()
            .iter()
            .all(|request| request.method == "GET")
    );
    assert!(
        fixture
            .requests()
            .iter()
            .all(|request| !request.headers.contains_key("authorization")
                && !request.headers.contains_key("cookie"))
    );
}

#[tokio::test]
async fn mai_aliases_are_exact_and_native_controls_are_not_replaced_by_personal_policy() {
    let fixture = Fixture::start(|request, _| match request.path.as_str() {
        "/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
        "/v1/models" => {
            Reply::json(json!({"data": [{"id": "gpt-5.6-sol"}, {"id": "gpt-5.6-luna"}]}))
        }
        "/v1/responses" => Reply::json(response(
            if request.json()["model"] == "gpt-5.6-sol" {
                "sol"
            } else {
                "luna"
            },
            "done",
        )),
        _ => panic!("MAI must not contact GitHub authentication or alternate endpoints"),
    })
    .await;
    let cache = cache();
    let backend = connect(&fixture, &cache, "").await;
    for (alias, mapped) in [
        ("gpt-5.6-sol-max", "gpt-5.6-sol"),
        ("gpt-5.6-luna-max", "gpt-5.6-luna"),
    ] {
        let value = backend
            .complete_responses(
                json!({
                    "model": alias, "input": "x", "max_output_tokens": 150_000,
                    "reasoning": {"effort": "low", "summary": "auto"},
                    "service_tier": "priority", "conversation": {"id": "mai-conversation"},
                    "context_management": [{"type": "provider-specific"}],
                    "tools": [
                        {"type": "image_generation"},
                        {"type": "namespace", "name": "image_gen", "tools": [
                            {"type": "function", "name": "imagegen", "parameters": {}}
                        ]},
                        {"type": "web_search"},
                        {"type": "function", "name": "lookup"}
                    ],
                    "store": false, "metadata": {"client": "fixture"},
                }),
                context(),
            )
            .await
            .unwrap();
        assert_eq!(value["store"], false);
        let requests = fixture.requests();
        let request = requests.last().unwrap();
        let payload = request.json();
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(payload["model"], mapped);
        assert_eq!(
            payload["reasoning"],
            json!({"effort": "max", "summary": "auto"})
        );
        assert_eq!(payload["max_output_tokens"], 128_000);
        assert_eq!(payload["service_tier"], "priority");
        assert_eq!(payload["conversation"], json!({"id": "mai-conversation"}));
        assert_eq!(
            payload["context_management"],
            json!([{"type": "provider-specific"}])
        );
        assert_eq!(
            payload["tools"],
            json!([{"type": "web_search"}, {"type": "function", "name": "lookup"}])
        );
        assert_eq!(payload["store"], false);
        assert!(!request.headers.contains_key("authorization"));
        assert!(!request.headers.contains_key("copilot-integration-id"));
        assert!(!request.headers.contains_key("x-api-key"));
    }
    assert_eq!(backend.history().statistics().unwrap()["entries"], 0);
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
async fn mai_native_stream_identity_and_history_use_the_same_safe_pipeline() {
    let fixture = Fixture::start(|request, _| match request.path.as_str() {
        "/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
        "/v1/models" => Reply::json(json!({"data": [{"id": "gpt-5.6-sol"}]})),
        "/v1/responses" => Reply::sse(&[
            json!({"type": "response.created", "response": {"id": "first", "output": []}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "id": "first_item", "type": "message", "role": "assistant", "status": "in_progress", "content": [],
            }}),
            json!({"type": "response.output_text.delta", "item_id": "delta_item", "output_index": 0, "content_index": 0, "delta": "done"}),
            json!({"type": "response.completed", "response": response("final", "done")}),
        ]),
        _ => panic!("unexpected MAI request"),
    }).await;
    let cache = cache();
    let backend = connect(&fixture, &cache, "").await;
    let events = backend
        .clone()
        .stream_responses(json!({"model": "gpt-5.6-sol", "input": "x"}), context())
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let result = &events.last().unwrap().data.as_ref().unwrap()["response"];
    assert_eq!(result["id"], "first");
    assert_eq!(result["output"][0]["id"], "first_item");
    assert_eq!(backend.history().read("first").unwrap().response, *result);
    assert_eq!(backend.history().statistics().unwrap()["entries"], 1);
}

#[tokio::test]
async fn mai_direct_chat_remains_chat_and_respects_cache_output_limits() {
    let fixture = Fixture::start(|request, _| match request.path.as_str() {
        "/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
        "/v1/models" => Reply::json(json!({"data": [{"id": "m"}]})),
        "/v1/chat/completions" => Reply::json(json!({
            "id": "chat_m", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "ok"}}],
            "provider_extension": {"retained": true},
        })),
        _ => panic!("Chat must not be converted to Responses"),
    }).await;
    let cache = CacheFile::new(&json!({"models": [{"slug": "m", "max_output_tokens": 64}]}));
    let backend = connect(&fixture, &cache, "").await;
    let result = backend.complete_chat(json!({
        "model": "m", "messages": [{"role": "user", "content": "hello"}],
        "max_completion_tokens": 100, "store": false, "reasoning_effort": "provider-specific",
    }), context()).await.unwrap();
    assert_eq!(result["object"], "chat.completion");
    assert_eq!(result["provider_extension"]["retained"], true);
    let request = fixture.requests().pop().unwrap();
    assert_eq!(request.json()["max_completion_tokens"], 64);
    assert_eq!(request.json()["reasoning_effort"], "provider-specific");
    assert_eq!(request.json()["store"], false);
    assert!(request.json().get("input").is_none());
    assert_eq!(backend.history().statistics().unwrap()["entries"], 0);
}

#[tokio::test]
async fn mai_wrong_identity_bad_metadata_and_unsafe_origins_fail_readiness() {
    for kind in ["identity", "models", "stream", "empty"] {
        let fixture = Fixture::start(move |request, _| match request.path.as_str() {
            "/" if kind == "identity" => Reply::json(json!({"message": "different application"})),
            "/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
            "/v1/models" if kind == "models" => Reply::json(json!({"data": [{"id": 1}]})),
            "/v1/models" if kind == "stream" => {
                Reply::raw(200, "text/event-stream", b"data: {}\n\n".to_vec())
            }
            "/v1/models" => Reply::json(json!({"data": []})),
            _ => panic!("readiness must not infer"),
        })
        .await;
        let cache = cache();
        let result = Backend::connect(
            BackendConfig::Mai {
                upstream: fixture.origin.to_string(),
                models_cache: cache.path.clone(),
            },
            context(),
        )
        .await;
        assert_eq!(result.unwrap_err().status, 502);
        assert!(
            fixture
                .requests()
                .iter()
                .all(|request| request.method == "GET")
        );
    }
    let cache = cache();
    for upstream in [
        "file:///provider",
        "http://user:secret@127.0.0.1:1",
        "http://127.0.0.1:1/path?token=secret",
        "http://127.0.0.1:1/%2e%2e/other",
    ] {
        let error = Backend::connect(
            BackendConfig::Mai {
                upstream: upstream.into(),
                models_cache: cache.path.clone(),
            },
            context(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.status, 400);
        assert!(!error.message.contains("secret"));
    }
}

#[tokio::test]
async fn mai_cache_is_read_bounded_without_overwriting_the_original() {
    let fixture = Fixture::start(|request, _| match request.path.as_str() {
        "/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
        "/v1/models" => Reply::json(json!({"data": [{"id": "m"}]})),
        _ => panic!("invalid cache must not infer"),
    })
    .await;
    let cache = CacheFile::new(&json!({"models": [{"slug": "m"}]}));
    for bytes in [b"{invalid fixture JSON".to_vec(), {
        let mut bytes = b"{\"models\":[{\"slug\":\"m\"}]}".to_vec();
        bytes.resize(16 * 1024 * 1024 + 1, b' ');
        bytes
    }] {
        std::fs::write(&cache.path, &bytes).unwrap();
        let error = Backend::connect(
            BackendConfig::Mai {
                upstream: fixture.origin.to_string(),
                models_cache: cache.path.clone(),
            },
            context(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(std::fs::read(&cache.path).unwrap(), bytes);
    }
    assert!(fixture.requests().iter().all(|request| request.path == "/"));
}

#[tokio::test]
async fn mai_error_status_and_redirects_do_not_trigger_other_upstreams_or_retries() {
    let target = Fixture::start(|_, _| panic!("redirects must never be followed")).await;
    let target_url = target.origin.to_string();
    let fixture = Fixture::start(move |request, _| match request.path.as_str() {
        "/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
        "/v1/models" => Reply::json(json!({"data": [{"id": "gpt-5.6-sol"}]})),
        "/v1/responses" if request.json()["input"] == "redirect" => {
            Reply::json(json!({"message": "redirect"}))
                .status(302)
                .header("Location", &target_url)
        }
        "/v1/responses" => Reply::json(json!({"error": {"message": "limited"}})).status(429),
        _ => panic!("unexpected request"),
    })
    .await;
    let cache = cache();
    let backend = connect(&fixture, &cache, "").await;
    for (input, status) in [("redirect", 502), ("limited", 429)] {
        assert_eq!(
            backend
                .complete_responses(json!({"model": "gpt-5.6-sol", "input": input}), context())
                .await
                .unwrap_err()
                .status,
            status
        );
    }
    assert_eq!(
        fixture
            .requests()
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        2
    );
    assert!(target.requests().is_empty());
    assert_eq!(backend.history().statistics().unwrap()["entries"], 0);
}

#[tokio::test]
async fn mai_compaction_requires_real_ciphertext_and_keeps_stored_history() {
    let fixture = Fixture::start(|request, _| match request.path.as_str() {
        "/" => Reply::json(json!({"message": "MAI LLM Proxy server is running"})),
        "/v1/models" => Reply::json(json!({"data": [{"id": "gpt-5.6-sol"}]})),
        "/v1/responses" => Reply::json(response("plain", "not encrypted")),
        _ => panic!("compaction must use the ordinary Responses endpoint"),
    })
    .await;
    let cache = cache();
    let backend = connect(&fixture, &cache, "").await;
    backend
        .complete_responses(
            json!({"model": "gpt-5.6-sol", "input": "remember"}),
            context(),
        )
        .await
        .unwrap();
    let error = backend
        .compact_responses(
            json!({
                "model": "gpt-5.6-sol", "previous_response_id": "plain", "input": "compact",
            }),
            context(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 502);
    assert!(error.message.contains("encrypted"));
    assert!(backend.history().read("plain").is_ok());
    assert_eq!(backend.history().statistics().unwrap()["entries"], 1);
    let request = fixture.requests().pop().unwrap();
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(
        request.json()["input"].as_array().unwrap().last().unwrap()["type"],
        "compaction_trigger"
    );
}
