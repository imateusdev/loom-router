use super::*;
use futures::StreamExt;
use tower::ServiceExt;

#[tokio::test]
async fn model_routes_acquire_the_wake_lock_but_health_checks_do_not() {
    let (wake, events) =
        crate::wake_lock::recording_controller(crate::config::SleepPreventionMode::WhileActive);
    wake.set_proxy_running(true);
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/responses", post(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(wake, track_model_activity));

    let health = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(health.status().is_success());
    assert!(events.try_recv().is_err());

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert!(events
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap());
}

#[tokio::test]
async fn an_unmatched_path_does_not_hold_the_wake_lock_open() {
    let (wake, events) =
        crate::wake_lock::recording_controller(crate::config::SleepPreventionMode::WhileActive);
    wake.set_proxy_running(true);
    let app = Router::new()
        .route("/v1/responses", post(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(wake, track_model_activity));

    let missing = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/embeddings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    // The lease is taken before routing can report the miss, so the backend does
    // acquire — but the 404 cancels it, so the release must follow immediately
    // instead of the 15-minute grace window holding the lock open.
    assert!(events
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap());
    assert!(!events
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap());
}

#[test]
fn ws_origin_policy_allows_only_local_browser_origins_or_no_origin() {
    // A regression here lets arbitrary webpages reach a localhost proxy that
    // holds provider credentials; the expected allowlist is intentionally
    // hand-written rather than derived from the implementation.
    for origin in [
        None,
        Some("http://localhost:1420"),
        Some("https://127.0.0.1:3000"),
        Some("http://tauri.localhost"),
    ] {
        let mut headers = HeaderMap::new();
        if let Some(origin) = origin {
            headers.insert("origin", origin.parse().unwrap());
        }
        assert!(is_trusted_ws_origin(&headers), "{origin:?} must be allowed");
    }

    for origin in [
        "https://example.com",
        "null",
        "https://localhost.evil.example",
        "file://localhost",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert("origin", origin.parse().unwrap());
        assert!(!is_trusted_ws_origin(&headers), "{origin} must be rejected");
    }
}

#[tokio::test]
async fn translated_stream_finalizes_when_upstream_closes_without_done() {
    // Removing the close-time finalize call leaves clients waiting forever
    // whenever a chat-compatible upstream closes after its last delta.
    let bytes = futures::stream::iter(vec![Ok::<_, reqwest::Error>(Bytes::from(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
    ))])
    .boxed();
    let frames = translate_byte_stream(
        bytes,
        UpstreamKind::OpenAiChat,
        DownstreamKind::Responses,
        "test-model",
        BTreeMap::new(),
        BTreeSet::new(),
        None,
    )
    .collect::<Vec<_>>()
    .await;
    let text = frames
        .into_iter()
        .map(Result::unwrap)
        .map(|frame| String::from_utf8(frame.to_vec()).unwrap())
        .collect::<String>();

    assert!(text.contains("event: response.output_text.delta"));
    assert!(text.contains("event: response.completed"));
}
