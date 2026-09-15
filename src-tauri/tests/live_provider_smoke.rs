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

/// Console Go rejects any request that arrives without `x-opencode-session`,
/// whatever the dialect. The whole reason `probe_model_dialect` attaches one
/// rests on that: without it the probe read the 400 as "this model does not
/// speak this wire", found no dialect for any Go model, and `toggle_model`
/// failed, which the UI showed as a checkbox reverting on its own.
///
/// If Go ever stops requiring the header the fix stays harmless, but if it
/// starts requiring something more than a freshly generated id, the probe
/// breaks again in exactly the same silent way. Prove both halves.
#[tokio::test]
async fn opencode_go_requires_the_session_header_is_live() {
    let Ok(key) = std::env::var("LOOM_ROUTER_OPENCODE_GO_API_KEY") else {
        eprintln!("skipping: LOOM_ROUTER_OPENCODE_GO_API_KEY is not set");
        return;
    };

    let client = reqwest::Client::new();
    let request = || {
        client
            .post("https://opencode.ai/zen/go/v1/chat/completions")
            .bearer_auth(&key)
            .json(&serde_json::json!({
                "model": "kimi-k3",
                "messages": [{"role": "user", "content": "Reply with OK."}],
                "max_tokens": 16,
                "stream": false,
            }))
    };

    let without = request()
        .send()
        .await
        .expect("live Console Go request failed");
    assert!(
        !without.status().is_success(),
        "Console Go accepted a request with no session header (status {}); \
         the probe no longer needs to synthesize one",
        without.status()
    );

    // A generated id, not a session the gateway already knows about: this is
    // what the probe sends, and it has no client turn to borrow one from.
    let with = request()
        .header("x-opencode-session", uuid::Uuid::new_v4().to_string())
        .send()
        .await
        .expect("live Console Go request failed");
    assert!(
        with.status().is_success(),
        "Console Go rejected a generated session id (status {})",
        with.status()
    );
}
