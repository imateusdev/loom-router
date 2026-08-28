use super::support::{provider, spawn, spawn_proxy};
use axum::{routing::post, Json, Router};
use loom_router_lib::config::{AppConfig, PromptCacheMode, ProviderProtocol};
use loom_router_lib::proxy;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

async fn routed_bodies(provider_id: &str, mode: Option<PromptCacheMode>) -> Vec<Value> {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&bodies);
    let upstream = Router::new().route(
        "/v1/messages",
        post(move |body: axum::body::Bytes| {
            let captured = Arc::clone(&captured);
            async move {
                captured
                    .lock()
                    .await
                    .push(serde_json::from_slice(&body).unwrap());
                Json(json!({
                    "id": "msg_test",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "ok"}],
                    "model": "claude-test",
                    "stop_reason": "end_turn",
                    "stop_sequence": null,
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                }))
            }
        }),
    );
    let upstream_url = spawn(upstream).await;
    let mut configured = provider(
        provider_id,
        "Anthropic compatible",
        ProviderProtocol::Anthropic,
        format!("{upstream_url}/v1"),
        "sk-test",
        "claude-test",
        None,
    );
    configured.prompt_cache = mode;
    let mut providers = BTreeMap::new();
    providers.insert(provider_id.to_string(), configured);
    let proxy_url = spawn_proxy(AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    })
    .await;
    let client = reqwest::Client::new();

    let responses = client
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-loomrouter-token", proxy::local_token())
        .json(&json!({
            "model": format!("{provider_id}/claude-test"),
            "input": [{"role": "user", "content": "hello"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    assert!(responses.status().is_success(), "responses route failed");

    let chat = client
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("x-loomrouter-token", proxy::local_token())
        .json(&json!({
            "model": format!("{provider_id}/claude-test"),
            "messages": [{"role": "user", "content": "hello"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    assert!(chat.status().is_success(), "chat route failed");

    let captured = bodies.lock().await.clone();
    captured
}

async fn routed_openai_bodies(provider_id: &str, mode: PromptCacheMode) -> Vec<Value> {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&bodies);
    let upstream = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::body::Bytes| {
            let captured = Arc::clone(&captured);
            async move {
                captured
                    .lock()
                    .await
                    .push(serde_json::from_slice(&body).unwrap());
                Json(json!({
                    "id": "chatcmpl_test",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }))
            }
        }),
    );
    let upstream_url = spawn(upstream).await;
    let mut configured = provider(
        provider_id,
        "OpenAI compatible",
        ProviderProtocol::OpenAI,
        format!("{upstream_url}/v1"),
        "sk-test",
        "chat-model",
        None,
    );
    configured.prompt_cache = Some(mode);
    let mut providers = BTreeMap::new();
    providers.insert(provider_id.to_string(), configured);
    let proxy_url = spawn_proxy(AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    })
    .await;
    let client = reqwest::Client::new();

    // The client's own breakpoint rides a content part, which is the only
    // place the upstreams read one from. A payload that put it at the top
    // level would not exercise the stripping a real client needs.
    for (path, payload) in [
        (
            "responses",
            json!({
                "model": format!("{provider_id}/chat-model"),
                "input": [{
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": "hello",
                        "cache_control": {"type": "ephemeral", "ttl": "client-supplied"}
                    }]
                }],
                "stream": false
            }),
        ),
        (
            "chat/completions",
            json!({
                "model": format!("{provider_id}/chat-model"),
                "messages": [{
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "hello",
                        "cache_control": {"type": "ephemeral", "ttl": "client-supplied"}
                    }]
                }],
                "stream": false
            }),
        ),
    ] {
        let response = client
            .post(format!("{proxy_url}/v1/{path}"))
            .header("x-loomrouter-token", proxy::local_token())
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{provider_id}/{path}");
    }

    let captured = bodies.lock().await.clone();
    captured
}

/// `cache_control` is a content-block property, read from the end of the last
/// message. It is not a request parameter — Anthropic defines no top-level
/// field by that name, so a marker placed there enables nothing.
fn breakpoint_of(body: &Value) -> Option<&Value> {
    assert!(
        body.get("cache_control").is_none(),
        "cache_control must never sit at the top level of the request:\n{body:#}"
    );
    body.get("messages")?
        .as_array()?
        .last()?
        .get("content")?
        .as_array()?
        .last()?
        .get("cache_control")
}

#[tokio::test]
async fn anthropic_prompt_cache_policy_reaches_real_upstream_on_both_http_routes() {
    let cases = [
        ("anthropic", None, Some(json!({"type": "ephemeral"}))),
        ("compatible", None, None),
        (
            "opencode-zen",
            Some(PromptCacheMode::OneHour),
            Some(json!({"type": "ephemeral", "ttl": "1h"})),
        ),
        ("compatible", Some(PromptCacheMode::OneHour), None),
        ("anthropic", Some(PromptCacheMode::Off), None),
    ];

    for (provider_id, mode, expected) in cases {
        let bodies = routed_bodies(provider_id, mode).await;
        assert_eq!(bodies.len(), 2, "{provider_id}");
        for body in bodies {
            assert_eq!(breakpoint_of(&body), expected.as_ref(), "{provider_id}");
        }
    }
}

#[tokio::test]
async fn provider_capabilities_control_real_openai_compatible_upstream_payloads() {
    let openrouter = routed_openai_bodies("openrouter", PromptCacheMode::OneHour).await;
    assert_eq!(openrouter.len(), 2);
    for body in openrouter {
        assert_eq!(
            breakpoint_of(&body),
            Some(&json!({"type": "ephemeral", "ttl": "1h"})),
            "openrouter takes the breakpoint on a content part"
        );
    }

    let deepseek = routed_openai_bodies("deepseek", PromptCacheMode::OneHour).await;
    assert_eq!(deepseek.len(), 2);
    for body in deepseek {
        assert_eq!(breakpoint_of(&body), None);
    }
}

/// The payloads `routed_openai_bodies` sends carry a client-supplied
/// `cache_control`. An automatic provider has to reach its upstream with none
/// of it left, wherever the client put it.
#[tokio::test]
async fn a_client_supplied_breakpoint_is_stripped_for_an_automatic_provider() {
    for body in routed_openai_bodies("deepseek", PromptCacheMode::OneHour).await {
        assert!(
            !body.to_string().contains("cache_control"),
            "an automatic provider must receive no cache directive at all:\n{body:#}"
        );
    }
}
