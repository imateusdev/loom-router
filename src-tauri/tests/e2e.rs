//! End-to-end proxy test: fake OpenAI-compatible upstream -> LoomRouter
//! proxy -> Responses API SSE stream.

use axum::{http::StatusCode, routing::post, Json, Router};
use loom_router_lib::config::{AppConfig, Provider, ProviderModel, ProviderProtocol};
use loom_router_lib::proxy;
use loom_router_lib::stats::Stats;
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
    // Spread the default so adding a config field does not break every
    // test that only cares about the port and the providers.
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;

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
    // Spread the default so adding a config field does not break every
    // test that only cares about the port and the providers.
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;

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
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(|_body: axum::body::Bytes| async {
            axum::Json(serde_json::json!({
                "id": "chatcmpl-large",
                "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            }))
        }),
    ).layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024));
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

#[tokio::test]
async fn responses_websocket_end_to_end() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

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
    // Spread the default so adding a config field does not break every
    // test that only cares about the port and the providers.
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;
    let ws_url = format!(
        "{}/v1/responses?token={}",
        proxy_url.replacen("http", "ws", 1),
        proxy::local_token()
    );

    // 2. Codex v2 handshake + response.create frame.
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    ws.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "test/m",
            "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    // 3. Events arrive one JSON object per WS text frame, ending with
    //    response.completed.
    let mut saw_created = false;
    let mut saw_delta_text = false;
    let mut saw_completed = false;
    while let Some(Ok(Message::Text(frame))) = ws.next().await {
        let event: serde_json::Value = serde_json::from_str(&frame).unwrap();
        match event["type"].as_str() {
            Some("response.created") => saw_created = true,
            Some("response.output_text.delta") => {
                if event["delta"].as_str() == Some("Hello") {
                    saw_delta_text = true;
                }
            }
            Some("response.completed") => {
                saw_completed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_created, "missing response.created");
    assert!(saw_delta_text, "missing output text delta");
    assert!(saw_completed, "missing response.completed");
    ws.close(None).await.unwrap();
}

/// DeepSeek-style upstream: reasoning_content then TWO parallel tool calls.
const TOOL_SSE: &str = concat!(
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"reasoning_content\":\"need to look\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"\"}},{\"index\":1,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}},{\"index\":1,\"function\":{\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// Regression: the Codex WS two-turn flow with PARALLEL tool calls. Turn 1
/// returns two function_call items; turn 2 re-sends previous_response_id with
/// both outputs. The translated chat body must keep both calls in ONE
/// assistant message followed by both tool messages — splitting them into
/// consecutive assistant messages makes Console Go 400 ("insufficient tool
/// messages following tool_calls message").
#[tokio::test]
async fn ws_parallel_tool_turn_rebuild_produces_valid_chat_messages() {
    use futures::{SinkExt, StreamExt};
    use std::sync::Mutex;
    use tokio_tungstenite::tungstenite::Message;

    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::body::Bytes| async move {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            cap.lock().unwrap().push(v.clone());
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(TOOL_SSE.to_string()))
                .unwrap()
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
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;
    let ws_url = format!(
        "{}/v1/responses?token={}",
        proxy_url.replacen("http", "ws", 1),
        proxy::local_token()
    );

    let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();

    // Turn 1: plain user turn; upstream answers with two parallel tool calls.
    ws.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "test/m",
            "input": [{"role":"user","content":[{"type":"input_text","text":"list files"}]}],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let mut resp1 = String::new();
    let mut call_ids: Vec<String> = Vec::new();
    while let Some(Ok(Message::Text(frame))) = ws.next().await {
        let ev: serde_json::Value = serde_json::from_str(&frame).unwrap();
        match ev["type"].as_str() {
            Some("response.output_item.done") => {
                if ev["item"]["type"] == "function_call" {
                    call_ids.push(ev["item"]["call_id"].as_str().unwrap().to_string());
                }
            }
            Some("response.completed") => {
                resp1 = ev["response"]["id"].as_str().unwrap().to_string();
                break;
            }
            _ => {}
        }
    }
    assert_eq!(call_ids.len(), 2, "expected two function_call items");
    assert!(!resp1.is_empty(), "turn 1 never completed");

    // Turn 2: both tool results arrive; Codex continues with previous_response_id.
    let outputs: Vec<serde_json::Value> = call_ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "type": "function_call_output",
                "call_id": id,
                "output": "ok",
            })
        })
        .collect();
    ws.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "test/m",
            "previous_response_id": resp1,
            "input": outputs,
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    while let Some(Ok(Message::Text(frame))) = ws.next().await {
        let ev: serde_json::Value = serde_json::from_str(&frame).unwrap();
        if ev["type"] == "response.completed" {
            break;
        }
    }
    ws.close(None).await.unwrap();

    let bodies = captured.lock().unwrap();
    assert!(
        bodies.len() >= 2,
        "expected two upstream requests, got {bodies:?}"
    );
    let turn2 = &bodies[1];
    let msgs = turn2["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[1]["role"], "assistant");
    let calls = msgs[1]["tool_calls"].as_array().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "parallel calls must share one assistant message:\n{}",
        serde_json::to_string_pretty(turn2).unwrap()
    );
    assert_eq!(msgs[2]["role"], "tool");
    assert_eq!(msgs[2]["tool_call_id"], call_ids[0]);
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], call_ids[1]);
}

/// Regression: Codex reconnects its WebSocket mid-conversation (idle
/// timeout, network blip). Turn 1 completes on connection A; turn 2 arrives
/// on a brand-new connection B carrying only `previous_response_id` + its
/// delta. The routed provider is stateless, so the proxy must rebuild the
/// full input from the history cache — which is shared across connections.
/// Before the fix the cache lived inside each WS session, so connection B
/// started empty, degraded to delta-only, and the upstream model lost the
/// whole conversation (context window reset to zero).
#[tokio::test]
async fn routed_ws_reconnect_keeps_the_conversation() {
    use futures::{SinkExt, StreamExt};
    use std::sync::Mutex;
    use tokio_tungstenite::tungstenite::Message;

    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::body::Bytes| async move {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            cap.lock().unwrap().push(v.clone());
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(UPSTREAM_SSE.to_string()))
                .unwrap()
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
                enabled: true,
                fast_mode: false,
                supports_vision: false,
            }],
            enabled: true,
        },
    );
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;
    let ws_url = format!(
        "{}/v1/responses?token={}",
        proxy_url.replacen("http", "ws", 1),
        proxy::local_token()
    );

    // Connection A: turn 1, plain user turn.
    let (mut ws_a, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    ws_a.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "test/m",
            "input": [{"role":"user","content":[{"type":"input_text","text":"first turn"}]}],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let mut resp1 = String::new();
    while let Some(Ok(Message::Text(frame))) = ws_a.next().await {
        let ev: serde_json::Value = serde_json::from_str(&frame).unwrap();
        if ev["type"] == "response.completed" {
            resp1 = ev["response"]["id"].as_str().unwrap().to_string();
            break;
        }
    }
    assert!(!resp1.is_empty(), "turn 1 never completed");
    // The connection drops (idle timeout / network blip).
    ws_a.close(None).await.unwrap();

    // Connection B: same thread, follow-up turn with only previous_response_id
    // + the delta. The shared history must rebuild the full conversation.
    let (mut ws_b, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    ws_b.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "test/m",
            "previous_response_id": resp1,
            "input": [{"role":"user","content":[{"type":"input_text","text":"second turn"}]}],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    while let Some(Ok(Message::Text(frame))) = ws_b.next().await {
        let ev: serde_json::Value = serde_json::from_str(&frame).unwrap();
        if ev["type"] == "response.completed" {
            break;
        }
    }
    ws_b.close(None).await.unwrap();

    // The upstream saw both turns: the rebuild must carry the whole history
    // (first user turn + assistant reply) not just the delta.
    let bodies = captured.lock().unwrap();
    assert!(
        bodies.len() >= 2,
        "expected two upstream requests, got {bodies:?}"
    );
    let turn2 = &bodies[1];
    let msgs = turn2["messages"].as_array().unwrap();
    let texts: Vec<&str> = msgs.iter().filter_map(|m| m["content"].as_str()).collect();
    assert!(
        texts.contains(&"first turn"),
        "turn 2 lost the first turn:\n{}",
        serde_json::to_string_pretty(turn2).unwrap()
    );
    assert!(
        texts.contains(&"second turn"),
        "turn 2 missing its own delta:\n{}",
        serde_json::to_string_pretty(turn2).unwrap()
    );
}

// ---------------------------------------------------------------------------
// Responses-protocol upstream (e.g. OpenCode Zen GPT models)
// ---------------------------------------------------------------------------

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
        Provider {
            id: "zen".into(),
            name: "Zen".into(),
            protocol: ProviderProtocol::Responses,
            base_url: format!("{upstream_url}/v1"),
            api_key: Some("sk-zen".into()),
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "gpt-5.5".into(),
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
    // Spread the default so adding a config field does not break every
    // test that only cares about the port and the providers.
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let stats = Arc::new(RwLock::new(Stats::in_memory()));
    let proxy_url = spawn(proxy::router(Arc::new(RwLock::new(config)), stats.clone())).await;

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
        Provider {
            id: "zen".into(),
            name: "Zen".into(),
            protocol: ProviderProtocol::Responses,
            base_url: format!("{upstream_url}/v1"),
            api_key: Some("sk-zen".into()),
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "gpt-5.5".into(),
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
    // Spread the default so adding a config field does not break every
    // test that only cares about the port and the providers.
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;

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

// ---------------------------------------------------------------------------
// Native -> routed switch with window clamp + anchored summary
// ---------------------------------------------------------------------------

/// Fake native upstream (ChatGPT backend shape): Responses SSE that completes
/// with a stable id, so the proxy caches the native turn under it.
const NATIVE_RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r_native_1\",\"status\":\"in_progress\"}}\n\n",
    "event: response.output_item.done\n",
    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"native reply\"}]}}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r_native_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":3,\"total_tokens\":14}}}\n\n",
);

/// A native turn (a model the proxy does not resolve) followed by a switch to
/// a ROUTED model with a tiny context window must: (1) keep the native turn
/// cached so the routed rebuild sees the full conversation, and (2) clamp the
/// oldest turns to the destination window, replacing them with an anchored
/// summary produced through the side-call fallback provider.
#[tokio::test]
async fn ws_native_turn_then_routed_switch_clamps_with_anchored_summary() {
    use futures::{SinkExt, StreamExt};
    use std::sync::Mutex;
    use tokio_tungstenite::tungstenite::Message;

    // 1. Fake native upstream (ChatGPT backend).
    let native_app = Router::new().route(
        "/responses",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                NATIVE_RESPONSES_SSE.to_string(),
            )
        }),
    );
    let native_url = spawn(native_app).await;
    let old_native = std::env::var("CODEX_NATIVE_BASE_URL").ok();
    std::env::set_var("CODEX_NATIVE_BASE_URL", &native_url);

    // 2. Fake fallback provider: answers the summary call with plain text.
    let fallback_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            axum::Json(serde_json::json!({
                "id": "summary-1",
                "choices": [{"message": {"role": "assistant", "content": "ANCHORED SUMMARY"}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }))
        }),
    );
    let fallback_url = spawn(fallback_app).await;

    // 3. Fake routed upstream: capture the request so we can assert on the
    //    clamped + summarized conversation, then answer normally.
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    let routed_app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::body::Bytes| async move {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            cap.lock().unwrap().push(v.clone());
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(UPSTREAM_SSE.to_string()))
                .unwrap()
        }),
    );
    let routed_url = spawn(routed_app).await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "test".to_string(),
        Provider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: format!("{routed_url}/v1"),
            api_key: Some("sk-test".into()),
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "m".into(),
                label: None,
                // Tiny window: any real conversation clamps immediately.
                context_window: Some(600),
                protocol: None,
                fast_mode: false,
                enabled: true,
                supports_vision: false,
            }],
            enabled: true,
        },
    );
    providers.insert(
        "fallback".to_string(),
        Provider {
            id: "fallback".into(),
            name: "Fallback".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: format!("{fallback_url}/v1"),
            api_key: Some("sk-fallback".into()),
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "fb".into(),
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
    let config = AppConfig {
        port: 0,
        providers,
        side_call_fallback: Some("fallback/fb".to_string()),
        ..AppConfig::default()
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;
    let ws_url = format!(
        "{}/v1/responses?token={}",
        proxy_url.replacen("http", "ws", 1),
        proxy::local_token()
    );
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();

    // Turn 1: NATIVE model. Not resolvable by the config, so it is forwarded
    // to the fake ChatGPT backend. The completed id must be cached.
    ws.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "gpt-native-sol",
            "input": [{"role":"user","content":[{"type":"input_text","text":"native turn with a lot of context to dump"}]}],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let mut native_completed = false;
    while let Some(Ok(Message::Text(frame))) = ws.next().await {
        let ev: serde_json::Value = serde_json::from_str(&frame).unwrap();
        if ev["type"] == "response.completed" {
            native_completed = true;
            break;
        }
    }
    assert!(native_completed, "native turn never completed");

    // Turn 2: SWITCH to the routed model, referencing the native turn.
    // The rebuild must resolve the native turn and clamp it to the window,
    // replacing it with the anchored summary from the fallback provider.
    ws.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "model": "test/m",
            "previous_response_id": "r_native_1",
            "input": [{"role":"user","content":[{"type":"input_text","text":"continue here"}]}],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    while let Some(Ok(Message::Text(frame))) = ws.next().await {
        let ev: serde_json::Value = serde_json::from_str(&frame).unwrap();
        if ev["type"] == "response.completed" {
            break;
        }
    }
    ws.close(None).await.unwrap();
    if let Some(old) = old_native {
        std::env::set_var("CODEX_NATIVE_BASE_URL", old);
    } else {
        std::env::remove_var("CODEX_NATIVE_BASE_URL");
    }

    // The routed upstream received exactly one clamped turn.
    let bodies = captured.lock().unwrap();
    assert_eq!(
        bodies.len(),
        1,
        "expected one routed request, got {bodies:?}"
    );
    let turn = &bodies[0];
    let msgs = turn["messages"].as_array().unwrap();
    // The clamp dropped the native turn and the anchored summary took its place.
    let first = msgs[0].to_string();
    assert!(
        first.contains("ANCHORED SUMMARY"),
        "anchored summary missing at the front:\n{first}"
    );
    let whole = msgs
        .iter()
        .map(|m| m.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !whole.contains("native turn with a lot of context to dump"),
        "native turn should have been clamped out:\n{whole}"
    );
    assert!(
        whole.contains("continue here"),
        "recent tail must survive the clamp:\n{whole}"
    );
}
