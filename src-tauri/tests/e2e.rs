//! End-to-end proxy test: fake OpenAI-compatible upstream -> LoomRouter
//! proxy -> Responses API SSE stream.

use axum::{routing::post, Router};
use loom_router_lib::config::{AppConfig, Provider, ProviderModel, ProviderProtocol};
use loom_router_lib::proxy;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

const UPSTREAM_SSE: &str = concat!(
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\n",
    "data: [DONE]\n\n",
);

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

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
        Provider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: format!("{upstream_url}/v1"),
            api_key: Some("sk-test".into()),
            models: vec![ProviderModel {
                id: "m".into(),
                label: None,
                enabled: true,
            }],
            enabled: true,
        },
    );
    let config = AppConfig {
        port: 0,
        providers,
        autostart_server: false,
    };
    let proxy_url = spawn(proxy::router(Arc::new(RwLock::new(config)))).await;

    // 3. Codex-style Responses API streaming request.
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
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
    assert!(body.contains("event: response.created"), "missing created:\n{body}");
    assert!(
        body.contains("event: response.output_text.delta"),
        "missing delta:\n{body}"
    );
    assert!(body.contains("\"Hello\""), "missing text:\n{body}");
    assert!(
        body.contains("event: response.completed"),
        "missing completed:\n{body}"
    );
    assert!(body.contains("\"input_tokens\":4"), "missing usage:\n{body}");
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
        Provider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: format!("{upstream_url}/v1"),
            api_key: Some("sk-test".into()),
            models: vec![ProviderModel {
                id: "m".into(),
                label: None,
                enabled: true,
            }],
            enabled: true,
        },
    );
    let config = AppConfig {
        port: 0,
        providers,
        autostart_server: false,
    };
    let proxy_url = spawn(proxy::router(Arc::new(RwLock::new(config)))).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
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
