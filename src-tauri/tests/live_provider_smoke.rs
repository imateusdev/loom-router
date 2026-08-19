//! Optional live-provider smoke tests.
//!
//! These self-skip unless the matching API key is present, so keyless CI and
//! local development stay green while a developer with real credentials can
//! prove the public endpoint still works.

#[tokio::test]
async fn deepseek_models_endpoint_is_live() {
    let Ok(key) = std::env::var("LOOM_ROUTER_DEEPSEEK_API_KEY") else {
        eprintln!("skipping: LOOM_ROUTER_DEEPSEEK_API_KEY is not set");
        return;
    };

    let client = reqwest::Client::new();
    let response = client
        .get("https://api.deepseek.com/v1/models")
        .bearer_auth(key)
        .send()
        .await
        .expect("live DeepSeek request failed");

    assert!(
        response.status().is_success(),
        "status: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.expect("models payload is not JSON");
    assert!(body.get("data").is_some_and(serde_json::Value::is_array));
}

/// MiniMax embeds thinking as `<think>` blocks inside `content` unless the
/// request carries `reasoning_split` (see `proxy::upstream`). If MiniMax ever
/// drops or renames that flag, nothing errors - the reasoning just silently
/// starts leaking into assistant text. Prove the flag still splits.
#[tokio::test]
async fn minimax_reasoning_split_is_live() {
    let Ok(key) = std::env::var("LOOM_ROUTER_MINIMAX_API_KEY") else {
        eprintln!("skipping: LOOM_ROUTER_MINIMAX_API_KEY is not set");
        return;
    };

    let response = reqwest::Client::new()
        .post("https://api.minimax.io/v1/chat/completions")
        .bearer_auth(key)
        .json(&serde_json::json!({
            "model": "MiniMax-M3",
            "reasoning_split": true,
            "stream": false,
            "max_completion_tokens": 300,
            "messages": [{"role": "user", "content": "What is 17*23? Think briefly."}],
        }))
        .send()
        .await
        .expect("live MiniMax request failed");

    assert!(
        response.status().is_success(),
        "status: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.expect("payload is not JSON");
    let message = &body["choices"][0]["message"];
    assert!(
        message["reasoning_content"].is_string(),
        "reasoning was not split out: {message}"
    );
    let content = message["content"].as_str().unwrap_or_default();
    assert!(
        !content.contains("<think>"),
        "thinking leaked into content: {content}"
    );
}
