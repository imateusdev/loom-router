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
