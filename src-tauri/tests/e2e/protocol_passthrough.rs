use super::support::{provider, spawn, spawn_proxy_with_stats};
use axum::{routing::post, Router};
use loom_router_lib::config::{AppConfig, ProviderProtocol};
use loom_router_lib::proxy;
use loom_router_lib::stats::Stats;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

const RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\"}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Zen hello\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":3,\"total_tokens\":14}}}\n\n",
);

#[tokio::test]
async fn responses_protocol_upstream_passthrough() {
    // 1. Fake upstream already speaking Responses SSE.
    let upstream_app = Router::new().route(
        "/v1/responses",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                RESPONSES_SSE.to_string(),
            )
        }),
    );
    let upstream_url = spawn(upstream_app).await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "zen".to_string(),
        provider(
            "zen",
            "Zen",
            ProviderProtocol::Responses,
            format!("{upstream_url}/v1"),
            "sk-zen",
            "gpt-5.5",
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
    let stats = Arc::new(RwLock::new(Stats::in_memory()));
    let proxy_url = spawn_proxy_with_stats(config, stats.clone()).await;

    // 2. Streaming turn: frames pass through untouched.
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-loomrouter-token", proxy::local_token())
        .json(&serde_json::json!({
            "model": "zen/gpt-5.5",
            "input": "hi",
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    assert!(body.contains("event: response.created"));
    assert!(body.contains("Zen hello"));
    assert!(body.contains("event: response.completed"));

    // 3. Usage was tapped into the stats db.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let log = stats.read().await.recent(10);
    assert_eq!(log.len(), 1, "expected one recorded request: {log:?}");
    assert_eq!(log[0].provider, "zen");
    assert_eq!(log[0].input_tokens, 11);
    assert_eq!(log[0].output_tokens, 3);
}

#[tokio::test]
async fn responses_protocol_upstream_non_stream() {
    let upstream_app = Router::new().route(
        "/v1/responses",
        post(|| async {
            axum::Json(serde_json::json!({
                "id": "r1",
                "object": "response",
                "status": "completed",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"pong"}]}],
                "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7}
            }))
        }),
    );
    let upstream_url = spawn(upstream_app).await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "zen".to_string(),
        provider(
            "zen",
            "Zen",
            ProviderProtocol::Responses,
            format!("{upstream_url}/v1"),
            "sk-zen",
            "gpt-5.5",
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
    let proxy_url = super::support::spawn_proxy(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-loomrouter-token", proxy::local_token())
        .json(&serde_json::json!({"model": "zen/gpt-5.5", "input": "ping"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["status"], "completed");
    assert_eq!(json["output"][0]["content"][0]["text"], "pong");
}
