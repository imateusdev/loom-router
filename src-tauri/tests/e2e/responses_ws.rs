use super::support::{provider, spawn, spawn_proxy, ws_url, UPSTREAM_SSE};
use axum::{routing::post, Router};
use futures::{SinkExt, StreamExt};
use loom_router_lib::config::{AppConfig, ProviderProtocol};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn responses_websocket_end_to_end() {
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
    let ws_url = ws_url(&proxy_url);

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
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let proxy_url = spawn_proxy(config).await;
    let ws_url = ws_url(&proxy_url);

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
    let config = AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    };
    let proxy_url = spawn_proxy(config).await;
    let ws_url = ws_url(&proxy_url);

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
        provider(
            "test",
            "Test",
            ProviderProtocol::OpenAI,
            format!("{routed_url}/v1"),
            "sk-test",
            "m",
            // Tiny window: any real conversation clamps immediately.
            Some(600),
        ),
    );
    providers.insert(
        "fallback".to_string(),
        provider(
            "fallback",
            "Fallback",
            ProviderProtocol::OpenAI,
            format!("{fallback_url}/v1"),
            "sk-fallback",
            "fb",
            None,
        ),
    );
    let config = AppConfig {
        port: 0,
        providers,
        side_call_fallback: Some("fallback/fb".to_string()),
        ..AppConfig::default()
    };
    let proxy_url = spawn_proxy(config).await;
    let ws_url = ws_url(&proxy_url);
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

/// Regression: a `response.cancel` sent while a turn is streaming must end the
/// turn with a terminal frame, and must leave the session usable.
///
/// Before the fix the cancel was never even read (the turn was awaited inline,
/// so the read loop was not polling) and was then dropped by a catch-all arm.
/// The client got no terminal event, so its turn state stayed open and every
/// later prompt on that connection looked like it was still thinking.
#[tokio::test]
async fn ws_cancel_ends_the_turn_and_keeps_the_session_usable() {
    // First turn stalls after one chunk; any later turn answers normally, so
    // the second turn proves the session survived the cancel.
    let calls = Arc::new(Mutex::new(0usize));
    let seen = calls.clone();
    let upstream_app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let seen = seen.clone();
            async move {
                let nth = {
                    let mut g = seen.lock().unwrap();
                    *g += 1;
                    *g
                };
                let body = if nth == 1 {
                    let head = futures::stream::once(async {
                        Ok::<_, std::io::Error>(axum::body::Bytes::from(
                            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"thinking\"},\"finish_reason\":null}]}\n\n",
                        ))
                    });
                    // Never completes within the test's lifetime.
                    let stall = futures::stream::once(async {
                        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                        Ok(axum::body::Bytes::from("data: [DONE]\n\n"))
                    });
                    axum::body::Body::from_stream(head.chain(stall))
                } else {
                    axum::body::Body::from(UPSTREAM_SSE.to_string())
                };
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(body)
                    .unwrap()
            }
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
    let proxy_url = spawn_proxy(AppConfig {
        port: 0,
        providers,
        ..AppConfig::default()
    })
    .await;
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url(&proxy_url))
        .await
        .unwrap();

    let create = |text: &str| {
        Message::Text(
            serde_json::json!({
                "type": "response.create",
                "model": "test/m",
                "input": [{"role":"user","content":[{"type":"input_text","text":text}]}],
            })
            .to_string()
            .into(),
        )
    };

    // Turn 1: wait until it is genuinely mid-stream before cancelling.
    ws.send(create("hi")).await.unwrap();
    let wait_for_delta = async {
        while let Some(Ok(Message::Text(frame))) = ws.next().await {
            let event: serde_json::Value = serde_json::from_str(&frame).unwrap();
            if event["type"] == "response.output_text.delta" {
                return;
            }
        }
        panic!("stream ended before any delta");
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), wait_for_delta)
        .await
        .expect("upstream never produced the first delta");

    ws.send(Message::Text(
        serde_json::json!({"type": "response.cancel"}).to_string().into(),
    ))
    .await
    .unwrap();

    // The cancel must produce a terminal frame; the upstream is still stalled,
    // so anything that arrives can only have come from the cancel path.
    let wait_for_terminal = async {
        while let Some(Ok(Message::Text(frame))) = ws.next().await {
            let event: serde_json::Value = serde_json::from_str(&frame).unwrap();
            if event["type"] == "response.incomplete" {
                return event;
            }
        }
        panic!("socket closed without a terminal frame");
    };
    let terminal = tokio::time::timeout(std::time::Duration::from_secs(10), wait_for_terminal)
        .await
        .expect("cancel produced no terminal frame: the client would hang forever");
    assert_eq!(
        terminal["response"]["incomplete_details"]["reason"], "cancelled",
        "terminal frame must say why the turn stopped:\n{terminal}"
    );

    // Turn 2 on the SAME connection must work.
    ws.send(create("again")).await.unwrap();
    let wait_for_completed = async {
        while let Some(Ok(Message::Text(frame))) = ws.next().await {
            let event: serde_json::Value = serde_json::from_str(&frame).unwrap();
            if event["type"] == "response.completed" {
                return;
            }
        }
        panic!("socket closed before the second turn completed");
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), wait_for_completed)
        .await
        .expect("session was poisoned by the cancel: the next turn never completed");
    ws.close(None).await.unwrap();
}
