//! End-to-end proxy test: fake OpenAI-compatible upstream -> LoomRouter
//! proxy -> Responses API SSE stream.

use axum::{routing::post, Router};
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
            user_agent: None,
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
        codex_integration: false,
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    )).await;

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
            user_agent: None,
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
        codex_integration: false,
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    )).await;

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
            user_agent: None,
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
        codex_integration: false,
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    )).await;
    let ws_url = format!("{}/v1/responses", proxy_url.replacen("http", "ws", 1));

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
            user_agent: None,
            models: vec![ProviderModel {
                id: "gpt-5.5".into(),
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
        codex_integration: false,
    };
    let stats = Arc::new(RwLock::new(Stats::in_memory()));
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        stats.clone(),
    ))
    .await;

    // 2. Streaming turn: frames pass through untouched.
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
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
            user_agent: None,
            models: vec![ProviderModel {
                id: "gpt-5.5".into(),
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
        codex_integration: false,
    };
    let proxy_url = spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .json(&serde_json::json!({"model": "zen/gpt-5.5", "input": "ping"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["status"], "completed");
    assert_eq!(json["output"][0]["content"][0]["text"], "pong");
}
