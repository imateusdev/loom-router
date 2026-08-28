//! Multi-key routing: primary selection, failover, rotation, and the
//! attribution every recorder hangs off the key that actually served a turn.

use super::dispatch::dispatch_routed;
use super::*;
use crate::config::{ProviderModel, ProviderProtocol};
use crate::keypool::FailureKind;

#[derive(Clone)]
struct TestUpstream {
    statuses: std::collections::HashMap<String, u16>,
    hits: Arc<Mutex<std::collections::HashMap<String, u32>>>,
}

async fn test_upstream_handler(
    axum::extract::Extension(upstream): axum::extract::Extension<TestUpstream>,
    headers: axum::http::HeaderMap,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    let auth = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_whitespace().last())
        .unwrap_or_default();
    let status = upstream.statuses.get(auth).copied().unwrap_or(200);
    let mut hits = upstream.hits.lock().unwrap_or_else(|e| e.into_inner());
    *hits.entry(auth.to_string()).or_insert(0) += 1;
    drop(hits);
    (
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::OK),
        axum::Json(serde_json::json!({
            "id": "resp_1",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "m",
            "output": [],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "input_tokens_details": {"cached_tokens": 0}
            }
        })),
    )
}

async fn spawn_test_upstream(upstream: TestUpstream) -> String {
    use axum::routing::post;
    let app = axum::Router::new()
        .route("/v1/responses", post(test_upstream_handler))
        .layer(axum::Extension(upstream));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn test_ctx(key_pools: KeyPools) -> ProxyCtx {
    ProxyCtx {
        config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        stats: Arc::new(tokio::sync::RwLock::new(crate::stats::Stats::in_memory())),
        key_pools,
        client: reqwest::Client::new(),
        history: Arc::new(Mutex::new(WsHistory::new())),
        wake: crate::wake_lock::WakeController::disabled(),
    }
}

fn keyed_provider(
    base_url: String,
    keys: Vec<crate::config::ProviderKey>,
    rotation_enabled: bool,
) -> Provider {
    Provider {
        id: "test".into(),
        name: "Test".into(),
        protocol: ProviderProtocol::Responses,
        base_url,
        api_key: None,
        keys,
        rotation_enabled,
        has_key: true,
        context_window: None,
        user_agent: None,
        prompt_cache: None,
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
    }
}

fn key(id: &str, secret: &str) -> crate::config::ProviderKey {
    crate::config::ProviderKey {
        id: id.into(),
        name: id.into(),
        enabled: true,
        api_key: Some(secret.into()),
        has_key: true,
    }
}

#[tokio::test]
async fn it_005_primary_key_is_used() {
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::new(),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream.clone()).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );

    let (_, key_id) = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();

    assert_eq!(key_id.as_deref(), Some("key-a"));
    let hits = upstream.hits.lock().unwrap();
    assert_eq!(hits.get("secret-a"), Some(&1));
    assert_eq!(hits.get("secret-b"), None);
}

#[tokio::test]
async fn it_006_failover_uses_key_b_and_cools_key_a() {
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::from([("secret-a".to_string(), 429)]),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream).await;
    let pools = KeyPools::new();
    let ctx = test_ctx(pools.clone());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );

    let (_, key_id) = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();
    let eligible = pools.eligible_keys(&provider, false).await;

    assert_eq!(key_id.as_deref(), Some("key-b"));
    assert_eq!(eligible[0].id, "key-b");
}

#[tokio::test]
async fn it_007_all_keys_fail_with_a_clear_error() {
    // "Clear" means the upstream's own status reaches the caller after
    // every key has been tried - a 429 flattened into a 502 is a rate
    // limit the client cannot back off from.
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::from([
            ("secret-a".to_string(), 429),
            ("secret-b".to_string(), 429),
        ]),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let hits = upstream.hits.clone();
    let url = spawn_test_upstream(upstream).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );

    let (response, key_id) = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(key_id.as_deref(), Some("key-b"));
    let hits = hits.lock().unwrap();
    assert_eq!(hits.get("secret-a"), Some(&1), "every key must be tried");
    assert_eq!(hits.get("secret-b"), Some(&1), "every key must be tried");
}

#[tokio::test]
async fn it_004_reorder_changes_the_primary_key() {
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::new(),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-b", "secret-b"), key("key-a", "secret-a")],
        false,
    );

    let (_, key_id) = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();

    assert_eq!(key_id.as_deref(), Some("key-b"));
}

#[tokio::test]
async fn it_009_rotation_round_robins_across_keys() {
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::new(),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        true,
    );

    let (_, first) = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();
    let (_, second) = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();

    assert_eq!(first.as_deref(), Some("key-a"));
    assert_eq!(second.as_deref(), Some("key-b"));
}

#[tokio::test]
async fn it_010_failover_stays_active_during_rotation() {
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::from([("secret-a".to_string(), 429)]),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        true,
    );

    let (_, key_id) = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();

    assert_eq!(key_id.as_deref(), Some("key-b"));
}

#[tokio::test]
async fn ut_019_each_key_is_tried_at_most_once() {
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::from([
            ("secret-a".to_string(), 429),
            ("secret-b".to_string(), 429),
        ]),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream.clone()).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );

    let (response, _) = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

    let hits = upstream.hits.lock().unwrap();
    assert_eq!(hits.get("secret-a"), Some(&1));
    assert_eq!(hits.get("secret-b"), Some(&1));
}

#[tokio::test]
async fn ut_021_all_keys_error_does_not_expose_key_values() {
    // Both ways `send` gives up: the upstream response it hands back, and
    // the error it builds when no attempt ever reached an upstream.
    // Neither may carry a key value.
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::from([
            ("secret-a".to_string(), 429),
            ("secret-b".to_string(), 429),
        ]),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );

    let (response, key_id) = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();
    assert_eq!(key_id.as_deref(), Some("key-b"));
    let body = response.text().await.unwrap();
    assert!(!body.contains("secret-a"));
    assert!(!body.contains("secret-b"));

    // Nothing answered at all, so the text is ours alone.
    let ctx = test_ctx(KeyPools::new());
    let unreachable = keyed_provider(
        "http://127.0.0.1:1/v1".to_string(),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );
    let error = send(&ctx, &unreachable, "responses", &json!({"model": "m"}))
        .await
        .unwrap_err()
        .to_string();

    assert!(!error.contains("secret-a"), "{error}");
    assert!(!error.contains("secret-b"), "{error}");
}

#[tokio::test]
async fn ut_023_all_5xx_returns_provider_failed_error() {
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::from([
            ("secret-a".to_string(), 500),
            ("secret-b".to_string(), 500),
        ]),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream).await;
    let pools = KeyPools::new();
    let ctx = test_ctx(pools.clone());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );

    let (response, _) = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();

    // A 500 is the provider's fault, and it says so with its own status.
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        pools.eligible_keys(&provider, false).await.is_empty(),
        "every key must be cooled down after a provider failure"
    );
}

#[test]
fn ut_017_quota_and_transient_statuses_classify_as_transient() {
    assert_eq!(
        classify_status(StatusCode::UNAUTHORIZED),
        Some(FailureKind::Auth)
    );
    assert_eq!(
        classify_status(StatusCode::FORBIDDEN),
        Some(FailureKind::Auth)
    );
    assert_eq!(
        classify_status(StatusCode::TOO_MANY_REQUESTS),
        Some(FailureKind::Transient)
    );
    assert_eq!(
        classify_status(StatusCode::PAYMENT_REQUIRED),
        Some(FailureKind::Transient)
    );
    assert_eq!(
        classify_status(StatusCode::INTERNAL_SERVER_ERROR),
        Some(FailureKind::Transient)
    );
}

#[test]
fn turn_finish_reason_separates_a_tool_call_from_just_talking() {
    // The case this exists for: the agent says what it is about to do and
    // hands control back without doing it. That turn is indistinguishable
    // from a normal answer in the log unless the outcome is recorded.
    let announced_and_stopped = json!({"response": {"status": "completed", "output": [
        {"type": "reasoning"},
        {"type": "message", "role": "assistant",
         "content": [{"type": "output_text", "text": "Vou reproduzir o white screen agora."}]}
    ]}});
    assert_eq!(
        turn_finish_reason(&announced_and_stopped).as_deref(),
        Some("stop")
    );

    let acted = json!({"response": {"status": "completed", "output": [
        {"type": "message", "role": "assistant", "content": []},
        {"type": "function_call", "name": "shell"}
    ]}});
    assert_eq!(turn_finish_reason(&acted).as_deref(), Some("tool_calls"));

    let cut_short = json!({"response": {"status": "incomplete", "output": []}});
    assert_eq!(
        turn_finish_reason(&cut_short).as_deref(),
        Some("incomplete")
    );

    // A chat upstream states it outright; that word wins over any guess.
    let chat = json!({"choices": [{"finish_reason": "length"}]});
    assert_eq!(turn_finish_reason(&chat).as_deref(), Some("length"));

    // Nothing to go on: record nothing rather than invent "stop".
    assert_eq!(turn_finish_reason(&json!({"id": "resp_1"})), None);
}

#[test]
fn ut_017b_malformed_request_statuses_never_blame_the_key() {
    // A bad request is not a bad key: cooling one down here parked the
    // only key for 25 minutes and reported "no enabled API key".
    assert_eq!(classify_status(StatusCode::BAD_REQUEST), None);
    assert_eq!(classify_status(StatusCode::NOT_FOUND), None);
    assert_eq!(classify_status(StatusCode::UNPROCESSABLE_ENTITY), None);
    assert_eq!(classify_status(StatusCode::PAYLOAD_TOO_LARGE), None);
}

#[tokio::test]
async fn it_011_dispatch_records_the_serving_key_id() {
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::from([("secret-a".to_string(), 429)]),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );
    let payload = json!({"model": "test/m", "input": "hi", "stream": false});

    let response = dispatch_routed(&ctx, &provider, "m", "test/m", &payload, WireApi::Responses)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let summary = ctx.stats.read().await.summarize(86_400);

    assert_eq!(summary.per_key[0].key_id, "key-b");
    assert_eq!(summary.per_key[0].requests, 1);
}

#[tokio::test]
async fn it_013_routed_logs_record_the_actual_upstream_model() {
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::new(),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(format!("{url}/v1"), vec![key("key-a", "secret-a")], false);
    let payload = json!({"model": "gpt-5.6-luna", "input": "hi", "stream": false});

    let response = dispatch_routed(
        &ctx,
        &provider,
        "deepseek-v4-flash",
        "gpt-5.6-luna",
        &payload,
        WireApi::Responses,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let logged = ctx.stats.read().await.recent(10);
    assert_eq!(logged[0].provider, "test");
    assert_eq!(logged[0].model, "test/deepseek-v4-flash");
}

#[tokio::test]
async fn it_012_a_rate_limited_routed_turn_keeps_its_status_and_is_logged() {
    // Every key rate-limited used to reach the client as a 502 with no row
    // in the request log at all: nothing to back off from, nothing to see.
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::from([
            ("secret-a".to_string(), 429),
            ("secret-b".to_string(), 429),
        ]),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );
    let payload = json!({"model": "test/m", "input": "hi", "stream": false});

    let response = dispatch_routed(&ctx, &provider, "m", "test/m", &payload, WireApi::Responses)
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let logged = ctx.stats.read().await.recent(10);
    assert_eq!(
        logged.len(),
        1,
        "the failure must appear in the request log"
    );
    assert_eq!(logged[0].status, "error");
    assert_eq!(logged[0].key_id.as_deref(), Some("key-b"));
    assert!(logged[0]
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("429"));
}

#[tokio::test]
async fn it_011b_a_bad_request_stops_at_the_first_key_and_keeps_the_pool_healthy() {
    // Replaying a malformed request against every key burns the pool for
    // a problem no key can fix, and the next request then fails with
    // "no enabled API key" instead of the upstream's real complaint.
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::from([
            ("secret-a".to_string(), 400),
            ("secret-b".to_string(), 400),
        ]),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let hits = upstream.hits.clone();
    let url = spawn_test_upstream(upstream).await;
    let pools = KeyPools::new();
    let ctx = test_ctx(pools.clone());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );
    let payload = json!({"model": "test/m", "input": "hi", "stream": false});

    let response = dispatch_routed(&ctx, &provider, "m", "test/m", &payload, WireApi::Responses)
        .await
        .expect("a 400 must surface, not be retried away");

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "the upstream status must survive"
    );
    assert_eq!(
        hits.lock().unwrap().len(),
        1,
        "only the first key may be tried"
    );
    assert_eq!(
        pools.eligible_keys(&provider, false).await.len(),
        2,
        "neither key may be cooled down by a malformed request"
    );
}

#[test]
fn ut_040_resolve_keeps_the_same_provider_after_keys_are_added() {
    let mut config = AppConfig::default();
    config.providers.insert(
        "test".into(),
        keyed_provider(
            "https://example.invalid/v1".into(),
            vec![key("key-a", "secret-a")],
            false,
        ),
    );

    let (provider, upstream) = resolve(&config, "test/m").unwrap();

    assert_eq!(provider.id, "test");
    assert_eq!(upstream, "m");
}

#[test]
fn ut_041_renaming_a_key_does_not_change_the_model_slug() {
    let mut config = AppConfig::default();
    let mut provider = keyed_provider(
        "https://example.invalid/v1".into(),
        vec![key("key-a", "secret-a")],
        false,
    );
    provider.keys[0].name = "Renamed".into();
    config.providers.insert(provider.id.clone(), provider);

    let (provider, upstream) = resolve(&config, "test/m").unwrap();

    assert_eq!(provider.id, "test");
    assert_eq!(upstream, "m");
}

#[tokio::test]
async fn ut_042_provider_with_no_enabled_key_returns_a_config_error() {
    let ctx = test_ctx(KeyPools::new());
    let mut provider = keyed_provider(
        "https://example.invalid/v1".into(),
        vec![key("key-a", "secret-a")],
        false,
    );
    provider.keys[0].enabled = false;

    let error = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("no enabled API key"));
}

#[tokio::test]
async fn ut_042c_send_outcome_keeps_network_facts_separate_from_status() {
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        "http://127.0.0.1:1/v1".into(),
        vec![key("key-a", "secret-a")],
        false,
    );

    let result = send_outcome(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();

    assert!(result.response.is_none());
    assert!(result.key_id.is_none());
    assert!(result.error.is_some());
    assert!(result.outcome.status.is_none());
    assert!(result.outcome.network_error.is_some());
    assert!(!result.outcome.timed_out);
}

#[tokio::test]
async fn ut_042b_all_keys_cooling_is_not_reported_as_a_config_error() {
    // The keys are configured and enabled; they are merely resting after a
    // burst of 429s. "No enabled API key" sends the user to a settings page
    // to fix a problem that is not there.
    let upstream = TestUpstream {
        statuses: std::collections::HashMap::from([
            ("secret-a".to_string(), 429),
            ("secret-b".to_string(), 429),
        ]),
        hits: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };
    let url = spawn_test_upstream(upstream).await;
    let ctx = test_ctx(KeyPools::new());
    let provider = keyed_provider(
        format!("{url}/v1"),
        vec![key("key-a", "secret-a"), key("key-b", "secret-b")],
        false,
    );

    // The first turn cools every key down.
    send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap();
    let error = send(&ctx, &provider, "responses", &json!({"model": "m"}))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("cooling down"), "{error}");
    assert!(!error.contains("no enabled API key"), "{error}");
}
