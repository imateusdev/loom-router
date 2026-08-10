use super::*;

/// What a routed WS turn passes to the SSE translator: the upstream dialect,
/// the routed model slug, and the request-derived tool maps (namespace +
/// freeform) needed to restore the Responses shape on the way back.
pub(super) type WsTranslatorConfig = (
    UpstreamKind,
    String,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeSet<String>,
);

pub(super) fn ws_translator_config(
    provider: &Provider,
    upstream_model: &str,
    model: &str,
    upstream_kind: UpstreamKind,
    payload: &Value,
) -> Option<WsTranslatorConfig> {
    match upstream_kind {
        UpstreamKind::Responses
            if !needs_responses_function_tool_compat(provider, upstream_model) =>
        {
            None
        }
        kind => Some((
            kind,
            model.to_string(),
            translate::tool_namespace_map(payload),
            translate::freeform_tool_names(payload),
        )),
    }
}

/// Parse an upstream SSE byte stream into Responses event objects. With a
/// translator, upstream chat/anthropic events are converted to the Responses
/// format; without one, the payloads pass through untouched.
pub(super) fn sse_values_stream(
    bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
    translator: Option<WsTranslatorConfig>,
) -> futures::stream::BoxStream<'static, Result<Value, String>> {
    struct St {
        bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
        parser: SseParser,
        translator: Option<StreamTranslator>,
        pending: VecDeque<Value>,
        upstream_done: bool,
        finalized: bool,
    }

    let state = St {
        bytes,
        parser: SseParser::new(),
        translator: translator.map(|(kind, model, namespaces, freeform)| {
            StreamTranslator::new(kind, DownstreamKind::Responses, &model)
                .with_tool_namespaces(namespaces)
                .with_freeform_tools(freeform)
        }),
        pending: VecDeque::new(),
        upstream_done: false,
        finalized: false,
    };

    futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(v) = st.pending.pop_front() {
                return Some((Ok(v), st));
            }
            if st.upstream_done {
                if !st.finalized {
                    st.finalized = true;
                    if let Some(t) = st.translator.as_mut() {
                        for f in t.finalize() {
                            if !f.done_marker {
                                st.pending.push_back(f.data);
                            }
                        }
                    }
                    continue;
                }
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => {
                    for ev in st.parser.push(&chunk) {
                        if let Some(t) = st.translator.as_mut() {
                            for f in t.push_event(ev.event.as_deref(), &ev.data) {
                                if !f.done_marker {
                                    st.pending.push_back(f.data);
                                }
                            }
                        } else if ev.data.trim() != "[DONE]" {
                            if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                                st.pending.push_back(v);
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    return Some((Err(format!("upstream stream error: {e}")), st));
                }
                None => st.upstream_done = true,
            }
        }
    })
    .boxed()
}

/// Forward an SSE stream byte-for-byte, tapping the frame that carries
/// usage so the turn is recorded without altering what the client receives.
///
/// Works for any upstream dialect: `translate::normalize_usage` knows where
/// each one puts its usage, so a Responses stream (`response.completed`) and
/// a Chat Completions stream (final chunk, top-level or per-choice) are both
/// handled here instead of needing a tap apiece.
pub(super) fn tap_usage_stream(
    upstream: reqwest::Response,
    kind: UpstreamKind,
    stats: SharedStats,
    provider: String,
    model: String,
    started: std::time::Instant,
    visual_assistance: Option<VisualAssistanceMetadata>,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    // P3: stats/provider/model/started live in the state struct (built once)
    // instead of being cloned on every SSE chunk.
    struct St {
        bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
        parser: SseParser,
        recorded: bool,
        kind: UpstreamKind,
        stats: SharedStats,
        provider: String,
        model: String,
        started: std::time::Instant,
        visual_assistance: Option<VisualAssistanceMetadata>,
    }
    let state = St {
        bytes: upstream.bytes_stream().boxed(),
        parser: SseParser::new(),
        recorded: false,
        kind,
        stats,
        provider,
        model,
        started,
        visual_assistance,
    };
    futures::stream::unfold(state, |mut st| async move {
        match st.bytes.next().await {
            Some(Ok(chunk)) => {
                if !st.recorded {
                    for ev in st.parser.push(&chunk) {
                        // Terminator frames ("[DONE]") are not JSON; skip them
                        // quietly rather than treating them as a parse failure.
                        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
                            continue;
                        };
                        if record_payload_usage(
                            &st.stats,
                            &st.provider,
                            &st.model,
                            "http",
                            Some(st.started),
                            st.kind,
                            &v,
                            st.visual_assistance.as_ref(),
                        ) {
                            st.recorded = true;
                            break;
                        }
                    }
                }
                Some((Ok(chunk), st))
            }
            Some(Err(e)) => Some((Err(std::io::Error::other(e)), st)),
            None => None,
        }
    })
}

/// Transform an upstream SSE byte stream into the downstream wire format.
/// When `tap` is set, completed Responses turns report their usage.
pub(super) fn translate_byte_stream(
    bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
    upstream_kind: UpstreamKind,
    downstream_kind: DownstreamKind,
    model: &str,
    tool_namespaces: std::collections::BTreeMap<String, String>,
    freeform_tools: std::collections::BTreeSet<String>,
    tap: Option<(
        SharedStats,
        String,
        String,
        std::time::Instant,
        Option<VisualAssistanceMetadata>,
    )>,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    struct St {
        bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
        parser: SseParser,
        translator: StreamTranslator,
        pending: VecDeque<Bytes>,
        upstream_done: bool,
        finalized: bool,
        tap: Option<(
            SharedStats,
            String,
            String,
            std::time::Instant,
            Option<VisualAssistanceMetadata>,
        )>,
    }

    let state = St {
        bytes,
        parser: SseParser::new(),
        translator: StreamTranslator::new(upstream_kind, downstream_kind, model)
            .with_tool_namespaces(tool_namespaces)
            .with_freeform_tools(freeform_tools),
        pending: VecDeque::new(),
        upstream_done: false,
        finalized: false,
        tap,
    };

    futures::stream::unfold(state, move |mut st| async move {
        loop {
            if let Some(b) = st.pending.pop_front() {
                return Some((Ok(b), st));
            }
            if st.upstream_done {
                if !st.finalized {
                    st.finalized = true;
                    for f in st.translator.finalize() {
                        if let Some((stats, prov, mdl, started, visual_assistance)) = &st.tap {
                            // Translated frames are canonical Responses shape.
                            record_payload_usage(
                                stats,
                                prov,
                                mdl,
                                "http",
                                Some(*started),
                                UpstreamKind::Responses,
                                &f.data,
                                visual_assistance.as_ref(),
                            );
                        }
                        push_frame(&mut st.pending, &f, downstream_kind);
                    }
                    continue;
                }
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => {
                    for ev in st.parser.push(&chunk) {
                        for f in st.translator.push_event(ev.event.as_deref(), &ev.data) {
                            if let Some((stats, prov, mdl, started, visual_assistance)) = &st.tap {
                                // Translated frames are canonical Responses shape.
                                record_payload_usage(
                                    stats,
                                    prov,
                                    mdl,
                                    "http",
                                    Some(*started),
                                    UpstreamKind::Responses,
                                    &f.data,
                                    visual_assistance.as_ref(),
                                );
                            }
                            push_frame(&mut st.pending, &f, downstream_kind);
                        }
                    }
                }
                Some(Err(e)) => {
                    // B3: a mid-stream upstream error used to fall through to
                    // finalize(), which emitted `response.completed` - the
                    // client saw a truncated turn marked as successful and
                    // nothing was recorded as a failure. Mirror the WS path:
                    // emit an explicit error event, record the failure, and
                    // skip finalize() so no `response.completed` follows.
                    tracing::warn!("upstream stream error: {e}");
                    let message = format!("upstream stream error: {e}");
                    if let Some((stats, prov, mdl, started, _)) = &st.tap {
                        record_failure(stats, prov, mdl, "http", Some(*started), &message);
                    }
                    let err_event = match downstream_kind {
                        DownstreamKind::Responses => json!({
                            "type": "error",
                            "status": 502,
                            "error": {"code": Value::Null, "message": message},
                        }),
                        DownstreamKind::ChatCompletions => json!({
                            "error": {"message": message, "type": "upstream_stream_error"},
                        }),
                    };
                    let bytes = match downstream_kind {
                        DownstreamKind::Responses => frame_with_event("error", &err_event),
                        DownstreamKind::ChatCompletions => frame_data(&err_event),
                    };
                    st.pending.push_back(Bytes::from(bytes));
                    if downstream_kind == DownstreamKind::ChatCompletions {
                        st.pending.push_back(Bytes::from(frame_done()));
                    }
                    st.upstream_done = true;
                    st.finalized = true; // skip finalize(): failed turns never complete
                }
                None => st.upstream_done = true,
            }
        }
    })
}

fn push_frame(pending: &mut VecDeque<Bytes>, f: &translate::OutFrame, downstream: DownstreamKind) {
    if f.done_marker {
        if downstream == DownstreamKind::ChatCompletions {
            pending.push_back(Bytes::from(frame_done()));
        }
        return;
    }
    let bytes = match (&f.event, downstream) {
        (Some(ev), DownstreamKind::Responses) => frame_with_event(ev, &f.data),
        _ => frame_data(&f.data),
    };
    pending.push_back(Bytes::from(bytes));
}
