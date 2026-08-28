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

#[tokio::test]
async fn anthropic_prompt_cache_policy_reaches_real_upstream_on_both_http_routes() {
    let cases = [
        ("anthropic", None, Some(json!({"type": "ephemeral"}))),
        ("compatible", None, None),
        (
            "compatible",
            Some(PromptCacheMode::OneHour),
            Some(json!({"type": "ephemeral", "ttl": "1h"})),
        ),
        ("anthropic", Some(PromptCacheMode::Off), None),
    ];

    for (provider_id, mode, expected) in cases {
        let bodies = routed_bodies(provider_id, mode).await;
        assert_eq!(bodies.len(), 2, "{provider_id}");
        for body in bodies {
            assert_eq!(
                body.get("cache_control"),
                expected.as_ref(),
                "{provider_id}"
            );
        }
    }
}
