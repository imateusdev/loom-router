use super::tests_routing::demo_config;
use super::*;

#[test]
fn structured_proxy_errors_stay_smaller_than_clippy_large_err_limit() {
    assert!(std::mem::size_of::<StructuredError>() < 128);
}

#[test]
fn finds_responses_data_and_remote_images_without_text_only_parts() {
    let payload = json!({
        "input": [
            {"role": "user", "content": [
                {"type": "input_text", "text": "compare these"},
                {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="},
                {"type": "input_image", "image_url": "https://images.example/diagram.jpg"}
            ]},
            {"role": "user", "content": [{"type": "input_text", "text": "no image here"}]}
        ]
    });

    let images = image_parts_in_payload(&payload, WireApi::Responses);

    assert_eq!(images.len(), 2);
    assert_eq!(images[0].image.url, "data:image/png;base64,aGVsbG8=");
    assert_eq!(images[0].image.mime_type.as_deref(), Some("image/png"));
    assert_eq!(images[1].image.url, "https://images.example/diagram.jpg");
    assert_eq!(images[1].image.mime_type, None);
}

#[test]
fn finds_multiple_chat_image_url_parts() {
    let payload = json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "What changed?"},
            {"type": "image_url", "image_url": {"url": "https://images.example/before.png"}},
            {"type": "image_url", "image_url": {"url": "https://images.example/after.webp"}}
        ]}]
    });

    let images = image_parts_in_payload(&payload, WireApi::ChatCompletions);

    assert_eq!(images.len(), 2);
    assert_eq!(images[0].image.url, "https://images.example/before.png");
    assert_eq!(images[1].image.url, "https://images.example/after.webp");
}

#[tokio::test]
async fn rejects_non_user_images_before_visual_provider_preparation() {
    // A missing visual model makes this a useful ordering assertion: the
    // role check must win before the visual provider chain is resolved.
    let mut cfg = demo_config(None);
    cfg.visual_assistance.enabled = true;
    let mut payload = json!({
        "input": [{"role": "developer", "content": [
            {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="}
        ]}]
    });
    let original = payload.clone();

    let error = prepare_visual_assistance(
        &reqwest::Client::new(),
        &cfg,
        &mut payload,
        WireApi::Responses,
        "cheap/mini",
    )
    .await
    .unwrap_err();

    assert!(error
        .to_string()
        .contains("visual assistance only supports image parts in user messages"));
    assert_eq!(payload, original);
}

#[test]
fn keeps_each_user_images_evidence_with_its_own_message() {
    let mut payload = json!({
        "input": [
            {"role": "user", "content": [
                {"type": "input_text", "text": "first image"},
                {"type": "input_image", "image_url": "https://images.example/first.png"}
            ]},
            {"role": "user", "content": [
                {"type": "input_text", "text": "second image"},
                {"type": "input_image", "image_url": "https://images.example/second.png"}
            ]}
        ]
    });
    let evidence = vec![
        (
            0,
            "<untrusted-image-evidence>first</untrusted-image-evidence>".to_string(),
        ),
        (
            1,
            "<untrusted-image-evidence>second</untrusted-image-evidence>".to_string(),
        ),
    ];

    enrich_payload_with_evidence(&mut payload, WireApi::Responses, &evidence).unwrap();

    assert_eq!(payload["input"][0]["content"][1]["text"], evidence[0].1);
    assert_eq!(payload["input"][1]["content"][1]["text"], evidence[1].1);
    assert!(image_parts_in_payload(&payload, WireApi::Responses).is_empty());
}

#[test]
fn enriches_only_user_content_and_removes_responses_images() {
    let mut payload = json!({
        "instructions": "do not change this system instruction",
        "input": [
            {"role": "developer", "content": [{"type": "input_text", "text": "developer text"}]},
            {"role": "user", "content": [
                {"type": "input_text", "text": "describe it"},
                {"type": "input_image", "image_url": "https://images.example/diagram.png"}
            ]}
        ]
    });
    let evidence = "<untrusted-image-evidence>OCR: Chart</untrusted-image-evidence>";

    enrich_payload_with_evidence(
        &mut payload,
        WireApi::Responses,
        &[(1, evidence.to_string())],
    )
    .unwrap();

    assert_eq!(
        payload["instructions"],
        "do not change this system instruction"
    );
    assert_eq!(payload["input"][0]["content"][0]["text"], "developer text");
    assert_eq!(payload["input"][1]["content"].as_array().unwrap().len(), 2);
    assert_eq!(payload["input"][1]["content"][0]["text"], "describe it");
    assert_eq!(payload["input"][1]["content"][1]["text"], evidence);
    assert!(image_parts_in_payload(&payload, WireApi::Responses).is_empty());
}

#[test]
fn enriches_chat_user_text_and_removes_only_image_parts() {
    let mut payload = json!({
        "messages": [
            {"role": "system", "content": "keep system"},
            {"role": "user", "content": [
                {"type": "text", "text": "read this"},
                {"type": "image_url", "image_url": {"url": "https://images.example/doc.png"}}
            ]}
        ]
    });
    let evidence = "<untrusted-image-evidence>OCR: Hello</untrusted-image-evidence>";

    enrich_payload_with_evidence(
        &mut payload,
        WireApi::ChatCompletions,
        &[(1, evidence.to_string())],
    )
    .unwrap();

    assert_eq!(payload["messages"][0]["content"], "keep system");
    assert_eq!(payload["messages"][1]["content"][0]["text"], "read this");
    assert_eq!(payload["messages"][1]["content"][1]["text"], evidence);
    assert!(image_parts_in_payload(&payload, WireApi::ChatCompletions).is_empty());
}

#[test]
fn visual_capability_uses_the_routed_model_configuration() {
    let mut cfg = demo_config(None);
    cfg.providers.get_mut("cheap").unwrap().models[0].supports_vision = true;

    assert!(model_supports_vision(&cfg, "cheap/mini").unwrap());
    cfg.providers.get_mut("cheap").unwrap().models[0].supports_vision = false;
    assert!(!model_supports_vision(&cfg, "cheap/mini").unwrap());
}

#[tokio::test]
async fn native_vision_destination_bypasses_an_unconfigured_visual_chain() {
    let mut cfg = demo_config(None);
    cfg.visual_assistance.enabled = true;
    cfg.providers.get_mut("cheap").unwrap().models[0].supports_vision = true;
    let mut payload = json!({
        "input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "https://images.example/native.png"}
        ]}]
    });
    let original = payload.clone();

    prepare_visual_assistance(
        &reqwest::Client::new(),
        &cfg,
        &mut payload,
        WireApi::Responses,
        "cheap/mini",
    )
    .await
    .unwrap();

    assert_eq!(payload, original);
}

#[tokio::test]
async fn disabled_assistance_preserves_a_text_only_request() {
    let cfg = demo_config(None);
    let mut payload = json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "describe"},
            {"type": "image_url", "image_url": {"url": "https://images.example/disabled.png"}}
        ]}]
    });
    let original = payload.clone();

    prepare_visual_assistance(
        &reqwest::Client::new(),
        &cfg,
        &mut payload,
        WireApi::ChatCompletions,
        "cheap/mini",
    )
    .await
    .unwrap();

    assert_eq!(payload, original);
}

#[tokio::test]
async fn disabled_assistance_preserves_images_for_an_uncatalogued_routed_model() {
    let cfg = demo_config(None);
    let mut payload = json!({
        "input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "https://images.example/uncatalogued.png"}
        ]}]
    });
    let original = payload.clone();

    prepare_visual_assistance(
        &reqwest::Client::new(),
        &cfg,
        &mut payload,
        WireApi::Responses,
        "cheap/not-in-models",
    )
    .await
    .unwrap();

    assert_eq!(payload, original);
}

#[tokio::test]
async fn exhausted_visual_chain_returns_before_the_text_only_payload_is_built() {
    let mut cfg = demo_config(None);
    cfg.visual_assistance.enabled = true;
    let mut payload = json!({
        "input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "https://images.example/failure.png"}
        ]}]
    });
    let original = payload.clone();

    let error = prepare_visual_assistance(
        &reqwest::Client::new(),
        &cfg,
        &mut payload,
        WireApi::Responses,
        "cheap/mini",
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("no primary model configured"));
    assert_eq!(payload, original);
}

#[tokio::test]
async fn visual_chain_errors_are_redacted_before_logs_and_gateway_responses() {
    let image_url = "https://private.example/secret-image.png";
    let prompt = "customer roadmap: do not disclose";
    let api_key = "sk-visual-test-secret";
    let chain_error = anyhow::Error::new(visual::VisualAnalysisFailure::new(
        format!(
            "visual assistance exhausted configured fallbacks: provider returned 503 for {image_url}; prompt={prompt}; authorization={api_key}"
        ),
        vec![visual::VisionAttempt {
            model: "vision/fallback".into(),
            retryable: true,
            status: Some(503),
            duration_ms: 1_700,
            error: format!("provider returned 503 for {image_url}; authorization={api_key}"),
        }],
    ));

    let stats = std::sync::Arc::new(tokio::sync::RwLock::new(crate::stats::Stats::in_memory()));
    let failure = visual_preparation_failure(
        &stats,
        "vision-provider",
        "vision/model",
        "http",
        std::time::Instant::now(),
        &chain_error,
    );
    let gateway = structured_error(StatusCode::BAD_GATEWAY, failure.to_string()).into_response();
    let gateway_body = String::from_utf8(
        axum::body::to_bytes(gateway.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let log = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some(entry) = stats.read().await.recent(1).into_iter().next() {
                return entry;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("visual preparation failure should reach request logs");

    for sensitive in [image_url, prompt, api_key] {
        assert!(!log.error.as_deref().unwrap_or_default().contains(sensitive));
        assert!(!gateway_body.contains(sensitive));
    }
    // The status and duration are safe to surface and are what tells a
    // provider refusal apart from a network blip; nothing else may join.
    assert_eq!(
        log.error.as_deref(),
        Some("visual assistance exhausted configured fallbacks (HTTP 503 after 1700ms)")
    );
    assert!(gateway_body.contains("visual assistance exhausted configured fallbacks"));
    assert!(gateway_body.contains("HTTP 503 after 1700ms"));
    let attempt = &log
        .visual_assistance
        .as_ref()
        .expect("exhausted visual chains retain attempt metadata")
        .attempts[0];
    assert_eq!(attempt.model, "vision/fallback");
    assert!(attempt.retryable);
    assert_eq!(attempt.status, Some(503));
    assert_eq!(attempt.duration_ms, 1_700);
    assert_eq!(attempt.error, "provider returned HTTP 503");
    assert!(!attempt.error.contains(image_url));
    assert!(!attempt.error.contains(api_key));
}
#[test]
fn finds_and_replaces_images_returned_by_a_tool_call() {
    // Codex's view_image tool answers under `output`, not `content`. An
    // image invisible here reaches an upstream that rejects the request
    // outright ("unknown variant `input_image`").
    let mut payload = json!({
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "look"}]},
            {"type": "function_call_output", "call_id": "view_image:1", "output": [
                {"type": "input_text", "text": "screenshot"},
                {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="}
            ]}
        ]
    });

    let images = image_parts_in_payload(&payload, WireApi::Responses);
    assert_eq!(images.len(), 1, "tool output image must be seen");
    assert_eq!(images[0].message_index, 1);
    validate_image_part_roles(&payload, WireApi::Responses)
        .expect("a tool result carries no role and must not be rejected");

    enrich_payload_with_evidence(
        &mut payload,
        WireApi::Responses,
        &[(1, "EVIDENCE".to_string())],
    )
    .unwrap();

    let dumped = serde_json::to_string(&payload).unwrap();
    assert!(!dumped.contains("input_image"), "{dumped}");
    assert!(dumped.contains("EVIDENCE"), "{dumped}");
}

#[test]
fn visual_failure_detail_distinguishes_a_refusal_from_a_network_blip() {
    let attempt = |status, duration_ms| visual::VisionAttempt {
        model: "vision/model".into(),
        retryable: false,
        status,
        duration_ms,
        error: String::new(),
    };
    let metadata = |attempts: Vec<visual::VisionAttempt>| VisualAssistanceMetadata {
        images: Vec::new(),
        attempts: attempts.iter().map(visual_attempt_provenance).collect(),
    };

    // The real case: no HTTP response at all, which used to be
    // indistinguishable from a provider refusal.
    assert_eq!(
        visual_failure_detail(Some(&metadata(vec![attempt(None, 4508)]))),
        " (no response after 4508ms)"
    );
    assert_eq!(
        visual_failure_detail(Some(&metadata(vec![attempt(Some(429), 120)]))),
        " (HTTP 429 after 120ms)"
    );
    assert_eq!(
        visual_failure_detail(Some(&metadata(vec![
            attempt(Some(500), 90),
            attempt(Some(503), 80)
        ]))),
        " (HTTP 503 after 80ms, 2 attempts)"
    );
    assert_eq!(visual_failure_detail(None), "");
    assert_eq!(visual_failure_detail(Some(&metadata(vec![]))), "");
}
