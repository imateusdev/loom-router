use super::support::spawn;
use axum::{http::StatusCode, routing::post, Json, Router};
use loom_router_lib::config::{AppConfig, Provider, ProviderModel, ProviderProtocol};
use loom_router_lib::proxy;
use loom_router_lib::stats::Stats;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::test]
async fn routed_http_requests_are_clamped_to_context_window() {
    let upstream_app = Router::new().route(
        "/v1/responses",
        post(|body: axum::body::Bytes| async move {
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let input = parsed["input"].as_array().map_or(0, Vec::len);
            if input > 20 {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": {"message": "input too large"}})),
                )
            } else {
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "object": "response",
                        "id": "resp_clamped",
                        "status": "completed",
                        "output": [],
                        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                    })),
                )
            }
        }),
    );
    let upstream_url = spawn(upstream_app).await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "test".to_string(),
        Provider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::Responses,
            base_url: format!("{upstream_url}/v1"),
            api_key: Some("sk-test".into()),
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "m".into(),
                label: None,
                context_window: Some(100_000),
                protocol: None,
                fast_mode: false,
                enabled: true,
                supports_vision: false,
            }],
            enabled: true,
        },
    );
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(AppConfig {
            port: 0,
            providers,
            ..AppConfig::default()
        })),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;

    let input: Vec<serde_json::Value> = (0..100)
        .map(|i| {
            serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": format!("turn {i}: {}", "x".repeat(20_000))}]
            })
        })
        .collect();
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-loomrouter-token", proxy::local_token())
        .json(&serde_json::json!({"model": "test/m", "input": input, "stream": false}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "clamped request was rejected");
}

#[tokio::test]
async fn routed_compaction_v2_returns_single_compaction_item() {
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(|body: axum::body::Bytes| async move {
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(
                !parsed.as_object().unwrap().contains_key("tools"),
                "compaction request must not carry tools"
            );
            let last = parsed["messages"].as_array().unwrap().last().unwrap();
            assert!(
                last["content"]
                    .as_str()
                    .unwrap()
                    .contains("CONTEXT CHECKPOINT COMPACTION"),
                "compaction prompt missing"
            );
            Json(serde_json::json!({
                "id": "chatcmpl-compact",
                "choices": [{
                    "message": {"role": "assistant", "content": "summary: finish the fix, then run tests"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
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
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "m".into(),
                label: None,
                context_window: None,
                protocol: None,
                fast_mode: false,
                enabled: true,
                supports_vision: false,
            }],
            enabled: true,
        },
    );
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(AppConfig {
            port: 0,
            providers,
            ..AppConfig::default()
        })),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-loomrouter-token", proxy::local_token())
        .json(&serde_json::json!({
            "model": "test/m",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "work so far"}]},
                {"type": "compaction_trigger"}
            ],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "compaction request was rejected"
    );
    let body = resp.text().await.unwrap();
    assert_eq!(
        body.matches("event: response.output_item.done").count(),
        1,
        "expected exactly one output item, got:\n{body}"
    );
    assert!(
        body.contains("\"type\":\"compaction\""),
        "compaction item missing:\n{body}"
    );
    let encoded = body
        .split("lr1:")
        .nth(1)
        .and_then(|part| part.split('"').next())
        .expect("compaction envelope missing");
    let decoded = loom_router_lib::translate::decode_compaction_summary(&format!("lr1:{encoded}"))
        .expect("compaction summary should decode");
    assert_eq!(decoded, "summary: finish the fix, then run tests");
    assert!(
        body.contains("event: response.completed"),
        "completion missing:\n{body}"
    );
}

#[tokio::test]
async fn routed_compaction_v2_works_over_websocket() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Json(serde_json::json!({
                "id": "chatcmpl-ws-compact",
                "choices": [{
                    "message": {"role": "assistant", "content": "ws summary"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
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
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "m".into(),
                label: None,
                context_window: None,
                protocol: None,
                fast_mode: false,
                enabled: true,
                supports_vision: false,
            }],
            enabled: true,
        },
    );
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(AppConfig {
            port: 0,
            providers,
            ..AppConfig::default()
        })),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;
    let ws_url = format!(
        "{}/v1/responses?token={}",
        proxy_url.replacen("http", "ws", 1),
        proxy::local_token()
    );
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();

    ws.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "test/m",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "work so far"}]},
                {"type": "compaction_trigger"}
            ],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let mut done_count = 0;
    let mut completed = false;
    while let Some(Ok(Message::Text(frame))) = ws.next().await {
        let event: serde_json::Value = serde_json::from_str(&frame).unwrap();
        if event["type"] == "response.output_item.done" {
            done_count += 1;
            assert_eq!(event["item"]["type"], "compaction");
        }
        if event["type"] == "response.completed" {
            completed = true;
            break;
        }
    }
    ws.close(None).await.unwrap();
    assert_eq!(done_count, 1, "expected exactly one compaction output item");
    assert!(completed, "compaction stream never completed");
}

#[tokio::test]
async fn routed_compaction_v2_sanitizes_summary_only_reasoning_for_responses_upstreams() {
    let upstream_app = Router::new().route(
        "/v1/responses",
        post(|body: axum::body::Bytes| async move {
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let input = parsed["input"].as_array().unwrap();
            let reasoning_missing_text = input.iter().any(|item| {
                item["type"] == "reasoning"
                    && item
                        .get("content")
                        .and_then(serde_json::Value::as_array)
                        .is_none_or(|parts| {
                            !parts.iter().any(|part| part["type"] == "reasoning_text")
                        })
            });
            let has_tool_call = input
                .iter()
                .any(|item| item["type"] == "function_call");
            if reasoning_missing_text && has_tool_call {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": {"message": "reasoning_text must be passed back"}})),
                );
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "object": "response",
                    "id": "resp_reasoning_ok",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "sanitized summary"}]
                    }],
                    "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8}
                })),
            )
        }),
    );
    let upstream_url = spawn(upstream_app).await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "test".to_string(),
        Provider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::Responses,
            base_url: format!("{upstream_url}/v1"),
            api_key: Some("sk-test".into()),
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "m".into(),
                label: None,
                context_window: None,
                protocol: None,
                fast_mode: false,
                enabled: true,
                supports_vision: false,
            }],
            enabled: true,
        },
    );
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(AppConfig {
            port: 0,
            providers,
            ..AppConfig::default()
        })),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .header("x-loomrouter-token", proxy::local_token())
        .json(&serde_json::json!({
            "model": "test/m",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run the tool"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "plan"}]},
                {"type": "function_call", "call_id": "call_1", "name": "ping", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
                {"type": "compaction_trigger"}
            ],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "sanitized compaction was rejected"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("\"type\":\"compaction\""),
        "compaction item missing:\n{body}"
    );
    assert_eq!(
        body.matches("event: response.output_item.done").count(),
        1,
        "expected exactly one output item:\n{body}"
    );
}
