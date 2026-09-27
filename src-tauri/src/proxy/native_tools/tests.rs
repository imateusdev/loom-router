use super::*;
use tower::ServiceExt;

async fn mock_native(
    status: StatusCode,
    content_type: &'static str,
) -> (Router, Arc<Mutex<Vec<(String, HeaderMap, Bytes)>>>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let seen = captured.clone();
    let upstream = Router::new().fallback(
        move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push((uri.to_string(), headers, body));
                (
                    status,
                    [
                        ("content-type", content_type),
                        ("retry-after", "7"),
                        ("x-request-id", "native-fixture"),
                        ("set-cookie", "must-not-be-forwarded=1"),
                    ],
                    Bytes::from_static(b"native-response"),
                )
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let mut ctx = super::super::tests_keys::test_ctx(KeyPools::new());
    ctx.native_base_url = format!("http://{addr}/backend-api/codex");
    (
        router_for_context(ctx, DEFAULT_MAX_REQUEST_BODY_BYTES),
        captured,
    )
}

fn request(path: &str, content_type: &str, body: &'static str) -> Request {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("x-loomrouter-token", local_token())
        .header("authorization", "Bearer caller-chatgpt-token")
        .header("chatgpt-account-id", "caller-account")
        .header("session_id", "caller-session")
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn tooling_compat_search_preserves_native_auth_body_and_response_headers() {
    let (app, captured) = mock_native(StatusCode::OK, "application/json").await;
    let input = "{ \"search_query\": [{\"q\":\"Codex\"}] }";
    let response = app
        .oneshot(request(
            "/v1/alpha/search?token=local-only",
            "application/json",
            input,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.headers()["x-request-id"], "native-fixture");
    assert!(!response.headers().contains_key("set-cookie"));
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "native-response"
    );
    let captured = captured.lock().unwrap();
    let (path, headers, body) = &captured[0];
    assert_eq!(path, "/backend-api/codex/alpha/search");
    assert_eq!(headers["authorization"], "Bearer caller-chatgpt-token");
    assert_eq!(headers["chatgpt-account-id"], "caller-account");
    assert_eq!(headers["session_id"], "caller-session");
    assert!(!headers.contains_key("x-loomrouter-token"));
    assert_eq!(body.as_ref(), input.as_bytes());
}

#[tokio::test]
async fn tooling_compat_native_routes_preserve_errors_and_media_types() {
    for (path, upstream_path, input_type, output_type) in [
        (
            "/v1/alpha/search",
            "/alpha/search",
            "application/json",
            "application/json",
        ),
        (
            "/v1/responses/compact",
            "/responses/compact",
            "application/json",
            "application/json",
        ),
        (
            "/v1/images/generations",
            "/images/generations",
            "application/json",
            "text/event-stream",
        ),
        (
            "/images/edits",
            "/images/edits",
            "multipart/form-data; boundary=test",
            "image/png",
        ),
    ] {
        let (app, captured) = mock_native(StatusCode::TOO_MANY_REQUESTS, output_type).await;
        let response = app.oneshot(request(path, input_type, "{}")).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{path}");
        assert_eq!(response.headers()["retry-after"], "7");
        assert_eq!(response.headers()["content-type"], output_type);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "native-response"
        );
        assert_eq!(
            captured.lock().unwrap()[0].0,
            format!("/backend-api/codex{upstream_path}")
        );
    }
}

#[tokio::test]
async fn tooling_compat_search_keeps_auth_json_and_route_boundaries() {
    let (app, captured) = mock_native(StatusCode::OK, "application/json").await;
    let mut unauthorized = request("/v1/alpha/search", "application/json", "{}");
    unauthorized.headers_mut().remove("x-loomrouter-token");
    assert_eq!(
        app.clone().oneshot(unauthorized).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let mut cross_site = request("/v1/alpha/search", "application/json", "{}");
    cross_site
        .headers_mut()
        .insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(cross_site).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(request("/v1/alpha/search", "application/json", "{"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.oneshot(request("/v1/alpha/arbitrary", "application/json", "{}"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(captured.lock().unwrap().is_empty());
}
