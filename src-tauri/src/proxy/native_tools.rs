use super::*;

pub(super) async fn handle_native_search(
    AxState(ctx): AxState<ProxyCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    enforce_json_post(&headers)?;
    parse_body(&body, "/v1/alpha/search")?;
    // This is a native Codex tool, not a model-provider request. Keep an exact
    // route instead of exposing an unrestricted authenticated backend proxy.
    let url = format!("{}/alpha/search", ctx.native_base_url.trim_end_matches('/'));
    let mut request = ctx
        .client
        .post(&url)
        .header("content-type", "application/json")
        .body(body);
    for name in NATIVE_FORWARD_HEADERS {
        if let Some(value) = headers.get(*name) {
            request = request.header(*name, value);
        }
    }
    let upstream = request.send().await.map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            upstream_unreachable_error(&url, &error, "ChatGPT/OpenAI").to_string(),
        )
    })?;
    tracing::info!(status = %upstream.status(), "native search passthrough");
    Ok(native_response(upstream))
}

pub(super) fn native_response(upstream: reqwest::Response) -> Response {
    // Native tools return JSON, SSE or binary data. Losing the media type or
    // content encoding makes a successful response unusable by the client.
    let mut response = Response::builder().status(upstream.status());
    for name in [
        "content-type",
        "content-encoding",
        "cache-control",
        "retry-after",
        "x-request-id",
    ] {
        if let Some(value) = upstream.headers().get(name) {
            response = response.header(name, value);
        }
    }
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap()
}

#[cfg(test)]
mod tests;
