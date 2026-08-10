use super::support::{provider, spawn, spawn_proxy, UPSTREAM_SSE};
use axum::{routing::post, Router};
use loom_router_lib::config::{AppConfig, ProviderProtocol};
use loom_router_lib::proxy;
use std::collections::BTreeMap;

#[tokio::test]
async fn responses_stream_end_to_end() {
    // 1. Fake upstream speaking chat.completion.chunk SSE.
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                UPSTREAM_SSE.to_string(),
            )
        }),
    );
    let upstream_url = spawn(upstream_app).await;

    // 2. Proxy config pointing at the fake upstream.
    let mut providers = BTreeMap::new();
    providers.insert(
        "test".to_string(),
        provider(
            "test",
            "Test",
            ProviderProtocol::OpenAI,
            format!("{upstream_url}/v1"),
            "sk-test",
            "m",
            None,
        ),
    );
    // Spread the default so adding a config field does not break every
    // test that only cares about the port and the providers.
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let proxy_url = spawn_proxy(config).await;

    // 3. Codex-style Responses API streaming request.
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-loomrouter-token", proxy::local_token())
        .json(&serde_json::json!({
            "model": "test/m",
            "input": "hi",
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();

    // 4. The downstream stream must be Responses API events.
    assert!(
        body.contains("event: response.created"),
        "missing created:\n{body}"
    );
    assert!(
        body.contains("event: response.output_text.delta"),
        "missing delta:\n{body}"
    );
    assert!(body.contains("\"Hello\""), "missing text:\n{body}");
    assert!(
        body.contains("event: response.completed"),
        "missing completed:\n{body}"
    );
    assert!(
        body.contains("\"input_tokens\":4"),
        "missing usage:\n{body}"
    );
}

#[tokio::test]
async fn responses_non_stream_end_to_end() {
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            axum::Json(serde_json::json!({
                "id": "chatcmpl-x",
                "created": 1,
                "choices": [{"message": {"role": "assistant", "content": "pong"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }))
        }),
    );
    let upstream_url = spawn(upstream_app).await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "test".to_string(),
        provider(
            "test",
            "Test",
            ProviderProtocol::OpenAI,
            format!("{upstream_url}/v1"),
            "sk-test",
            "m",
            None,
        ),
    );
    // Spread the default so adding a config field does not break every
    // test that only cares about the port and the providers.
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let proxy_url = spawn_proxy(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-loomrouter-token", proxy::local_token())
        .json(&serde_json::json!({"model": "test/m", "input": "ping"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["object"], "response");
    assert_eq!(json["status"], "completed");
    assert_eq!(json["output"][0]["content"][0]["text"], "pong");
}

#[tokio::test]
async fn responses_accepts_compaction_payload_above_default_axum_limit() {
    let upstream_app = Router::new()
        .route(
            "/v1/chat/completions",
            post(|_body: axum::body::Bytes| async {
                axum::Json(serde_json::json!({
                    "id": "chatcmpl-large",
                    "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                }))
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024));
    let upstream_url = spawn(upstream_app).await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "test".to_string(),
        provider(
            "test",
            "Test",
            ProviderProtocol::OpenAI,
            format!("{upstream_url}/v1"),
            "sk-test",
            "m",
            None,
        ),
    );
    let proxy_url = spawn_proxy(AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    })
    .await;

    let large_input = "x".repeat(3 * 1024 * 1024);
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-loomrouter-token", proxy::local_token())
        .json(&serde_json::json!({
            "model": "test/m",
            "input": large_input,
            "stream": false,
        }))
        .send()
        .await
        .unwrap();

    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert!(status.is_success(), "unexpected response: {status} {body}");
}
