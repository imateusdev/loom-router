use super::*;
use crate::config::{AppConfig, PromptCacheMode, ProviderModel, ProviderProtocol};
use std::collections::BTreeMap;

/// One cheap provider serving `cheap/mini`; `fallback` maps to
/// `AppConfig.side_call_fallback`.
pub(super) fn demo_config(fallback: Option<&str>) -> AppConfig {
    let mut providers = BTreeMap::new();
    providers.insert(
        "cheap".into(),
        Provider {
            id: "cheap".into(),
            name: "Cheap".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: "https://api.cheap.example/v1".into(),
            api_key: Some("sk-test".into()),
            keys: vec![],
            rotation_enabled: false,
            has_key: true,
            context_window: None,
            user_agent: None,
            prompt_cache: None,
            models: vec![ProviderModel {
                id: "mini".into(),
                label: None,
                context_window: None,
                protocol: None,
                fast_mode: false,
                enabled: true,
                supports_vision: false,
            }],
            enabled: true,
        },
    );
    AppConfig {
        providers,
        side_call_fallback: fallback.map(str::to_string),
        // Other fields evolve in parallel; take their defaults.
        ..Default::default()
    }
}

/// The OpenCode shape: one URL, one key, three dialects — the dialect
/// recorded per model.
pub(super) fn multi_dialect_provider() -> Provider {
    let model = |id: &str, protocol: Option<ProviderProtocol>| ProviderModel {
        id: id.into(),
        label: None,
        context_window: None,
        protocol,
        fast_mode: false,
        enabled: true,
        supports_vision: false,
    };
    Provider {
        id: "opencode-go".into(),
        name: "OpenCode Go".into(),
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://opencode.ai/zen/go/v1".into(),
        api_key: Some("sk-test".into()),
        keys: vec![],
        rotation_enabled: false,
        has_key: true,
        context_window: None,
        user_agent: None,
        prompt_cache: None,
        models: vec![
            model("kimi-k3", Some(ProviderProtocol::OpenAI)),
            model("qwen3.8-max", Some(ProviderProtocol::Anthropic)),
            model("gpt-5.6-luna", Some(ProviderProtocol::Responses)),
            model("deepseek-v4-flash", Some(ProviderProtocol::Responses)),
            // Turned up by discovery, never given a dialect.
            model("something-new", None),
        ],
        enabled: true,
    }
}

#[test]
fn legacy_opencode_slugs_resolve_to_the_merged_provider() {
    // Threads saved before the provider merge still address
    // `opencode-go-chat/<model>`. Without the alias they fell into the
    // native passthrough and the ChatGPT backend rejected the turn with
    // 400, resetting the conversation.
    let mut providers = BTreeMap::new();
    let provider = multi_dialect_provider();
    providers.insert("opencode-go".to_string(), provider);
    let cfg = AppConfig {
        providers,
        ..Default::default()
    };

    for slug in [
        "opencode-go-chat/kimi-k3",
        "opencode-go-claude/qwen3.8-max",
        "opencode-go-responses/gpt-5.6-luna",
    ] {
        let (p, upstream) = resolve(&cfg, slug).expect(slug);
        assert_eq!(
            p.id, "opencode-go",
            "{slug} must resolve to the merged provider"
        );
        assert_eq!(upstream, slug.rsplit_once('/').unwrap().1);
    }
}

#[test]
fn legacy_opencode_slug_keeps_the_models_own_dialect() {
    let mut providers = BTreeMap::new();
    providers.insert("opencode-go".to_string(), multi_dialect_provider());
    let cfg = AppConfig {
        providers,
        ..Default::default()
    };
    let (p, upstream) = resolve(&cfg, "opencode-go-chat/gpt-5.6-luna").unwrap();
    // The merged provider records the Responses dialect on the model, so
    // the chat-slug's old meaning is not resurrected by the alias.
    assert_eq!(model_protocol(p, &upstream), &ProviderProtocol::Responses);
}
#[test]
fn merged_opencode_provider_ignores_unrelated_slugs() {
    let mut providers = BTreeMap::new();
    providers.insert("opencode-go".to_string(), multi_dialect_provider());
    let cfg = AppConfig {
        providers,
        ..Default::default()
    };
    // No legacy suffix: no alias.
    assert_eq!(merged_opencode_provider(&cfg, "opencode-go"), None);
    // A repointed provider id (not a gateway name) is not aliased.
    assert_eq!(merged_opencode_provider(&cfg, "opencode-go-custom"), None);
    // Missing merged provider: the alias must not invent one.
    assert_eq!(merged_opencode_provider(&cfg, "opencode-zen-chat"), None);
}

#[test]
fn each_model_resolves_its_own_dialect() {
    let p = multi_dialect_provider();
    assert_eq!(model_protocol(&p, "kimi-k3"), &ProviderProtocol::OpenAI);
    assert_eq!(
        model_protocol(&p, "qwen3.8-max"),
        &ProviderProtocol::Anthropic
    );
    assert_eq!(
        model_protocol(&p, "gpt-5.6-luna"),
        &ProviderProtocol::Responses
    );
    // Untagged, and unknown to the provider entirely: the provider's own
    // dialect is the only answer available.
    assert_eq!(
        model_protocol(&p, "something-new"),
        &ProviderProtocol::OpenAI
    );
    assert_eq!(model_protocol(&p, "never-seen"), &ProviderProtocol::OpenAI);
}

#[test]
fn the_shipped_zen_preset_does_not_capture_the_native_gpt_slugs() {
    // Not a hypothetical collision: the OpenCode Zen preset serves models
    // under the native names verbatim, so anyone who adds it and then asks
    // Codex for GPT-5.5 is asking a question the bare-name lookup used to
    // answer with OpenCode. Built from PRESETS so a future preset that
    // adds a native name fails here rather than in someone's session.
    let preset = crate::providers::PRESETS
        .iter()
        .find(|p| p.id == "opencode-zen")
        .expect("opencode-zen preset");
    let provider = Provider::from_preset(preset);
    let mut cfg = AppConfig::default();
    assert!(!cfg.native_slug_mode, "normal mode is the default");
    cfg.providers.insert(provider.id.clone(), provider);

    for bare in ["gpt-5.5", "gpt-5.4-mini", "gpt-5.4-nano", "grok-4.5"] {
        assert!(
            matches!(
                resolve_effective(&cfg, bare, &json!({"model": bare}), None),
                EffectiveRoute::Native
            ),
            "bare {bare} was captured by a routed provider"
        );
    }
    // The Zen copies stay reachable under their qualified slug, which is
    // what the picker publishes for them.
    let (p, upstream) = resolve(&cfg, "opencode-zen/gpt-5.5").unwrap();
    assert_eq!(p.id, "opencode-zen");
    assert_eq!(upstream, "gpt-5.5");
}

#[test]
fn one_provider_dispatches_each_model_to_its_own_upstream() {
    // The whole point of merging the per-dialect providers: the same
    // provider, key and URL must still reach three different endpoints.
    let p = multi_dialect_provider();
    let payload = json!({"input": [], "stream": false});
    let route = |model: &str| {
        let (path, _body, kind) = build_upstream(&p, &payload, model, WireApi::Responses).unwrap();
        (path, kind)
    };
    assert_eq!(
        route("kimi-k3"),
        ("chat/completions", UpstreamKind::OpenAiChat)
    );
    assert_eq!(route("qwen3.8-max"), ("messages", UpstreamKind::Anthropic));
    assert_eq!(
        route("gpt-5.6-luna"),
        ("responses", UpstreamKind::Responses)
    );
}

#[test]
fn zen_preset_dispatches_deepseek_v4_flash_over_chat_completions() {
    // Zen serves the flash tier through Chat Completions: /v1/responses on
    // https://opencode.ai/zen/v1 returns 500 even though the id is in the
    // catalog. Go, by contrast, keeps Responses because that is the dialect
    // its gateway exposes. Anchoring the dispatch on the shipped preset
    // means a regression that puts the flash tier back on Responses will
    // fail here, before any user request.
    let preset = crate::providers::PRESETS
        .iter()
        .find(|p| p.id == "opencode-zen")
        .expect("opencode-zen preset");
    let provider = crate::config::Provider::from_preset(preset);
    let payload = json!({"input": [], "stream": false});
    let (path, _body, kind) =
        build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();
    assert_eq!(path, "chat/completions");
    assert_eq!(kind, UpstreamKind::OpenAiChat);

    let go_preset = crate::providers::PRESETS
        .iter()
        .find(|p| p.id == "opencode-go")
        .expect("opencode-go preset");
    let go_provider = crate::config::Provider::from_preset(go_preset);
    let (go_path, _go_body, go_kind) = build_upstream(
        &go_provider,
        &payload,
        "deepseek-v4-flash",
        WireApi::Responses,
    )
    .unwrap();
    assert_eq!(go_path, "responses");
    assert_eq!(go_kind, UpstreamKind::Responses);
}

/// The cache breakpoint is a content-block property and lands on the last
/// block of the last message. Anthropic defines no top-level `cache_control`
/// request parameter, so one placed there would enable nothing at all.
fn breakpoint_of(body: &Value) -> Option<&Value> {
    assert!(
        body.get("cache_control").is_none(),
        "cache_control must never sit at the top level of the request:\n{body:#}"
    );
    body.get("messages")?
        .as_array()?
        .last()?
        .get("content")?
        .as_array()?
        .last()?
        .get("cache_control")
}

#[test]
fn official_anthropic_defaults_to_five_minute_prompt_cache() {
    let preset = crate::providers::PRESETS
        .iter()
        .find(|preset| preset.id == "anthropic")
        .expect("anthropic preset");
    let provider = Provider::from_preset(preset);
    let (_, body, _) = build_upstream(
        &provider,
        &json!({"messages": [{"role": "user", "content": "hello"}]}),
        "claude-opus-4-1",
        WireApi::ChatCompletions,
    )
    .unwrap();

    assert_eq!(breakpoint_of(&body), Some(&json!({"type": "ephemeral"})));
}

#[test]
fn anthropic_prompt_cache_policy_supports_off_and_one_hour() {
    let mut provider = multi_dialect_provider();
    let payload = json!({"messages": [{"role": "user", "content": "hello"}]});

    let (_, default_body, _) =
        build_upstream(&provider, &payload, "qwen3.8-max", WireApi::ChatCompletions).unwrap();
    assert_eq!(breakpoint_of(&default_body), None);

    provider.prompt_cache = Some(PromptCacheMode::OneHour);
    let (_, one_hour_body, _) =
        build_upstream(&provider, &payload, "qwen3.8-max", WireApi::ChatCompletions).unwrap();
    assert_eq!(
        breakpoint_of(&one_hour_body),
        Some(&json!({"type": "ephemeral", "ttl": "1h"}))
    );

    provider.prompt_cache = Some(PromptCacheMode::Off);
    let (_, off_body, _) =
        build_upstream(&provider, &payload, "qwen3.8-max", WireApi::ChatCompletions).unwrap();
    assert_eq!(breakpoint_of(&off_body), None);
}

#[test]
fn openrouter_applies_explicit_prompt_cache_to_chat_and_responses_upstreams() {
    let preset = crate::providers::PRESETS
        .iter()
        .find(|preset| preset.id == "openrouter")
        .expect("openrouter preset");
    let mut provider = Provider::from_preset(preset);
    provider.prompt_cache = Some(PromptCacheMode::OneHour);
    provider.models = vec![ProviderModel {
        id: "native-responses".into(),
        label: None,
        context_window: None,
        protocol: Some(ProviderProtocol::Responses),
        fast_mode: false,
        enabled: true,
        supports_vision: false,
    }];

    let (_, chat_body, _) = build_upstream(
        &provider,
        &json!({"messages": [{"role": "user", "content": "hello"}]}),
        "chat-model",
        WireApi::ChatCompletions,
    )
    .unwrap();
    assert_eq!(
        breakpoint_of(&chat_body),
        Some(&json!({"type": "ephemeral", "ttl": "1h"}))
    );

    // A model served in the Responses dialect gets no breakpoint. OpenRouter
    // documents the block-level `cache_control` for the chat wire; where it
    // belongs in a Responses payload — or whether it is read there at all —
    // is not documented, and inventing a placement would ship a field the
    // upstream never defined. Deliberate, and pinned so it stays deliberate.
    let (_, responses_body, _) = build_upstream(
        &provider,
        &json!({"input": [{"role": "user", "content": "hello"}]}),
        "native-responses",
        WireApi::Responses,
    )
    .unwrap();
    assert!(
        !responses_body.to_string().contains("cache_control"),
        "no cache directive should reach the Responses wire:\n{responses_body:#}"
    );
}

#[test]
fn automatic_cache_providers_never_receive_an_explicit_cache_field() {
    let preset = crate::providers::PRESETS
        .iter()
        .find(|preset| preset.id == "deepseek")
        .expect("deepseek preset");
    let mut provider = Provider::from_preset(preset);
    provider.prompt_cache = Some(PromptCacheMode::OneHour);
    let (_, body, _) = build_upstream(
        &provider,
        &json!({
            "messages": [{"role": "user", "content": "hello"}],
            "cache_control": {"type": "ephemeral", "ttl": "client-supplied"}
        }),
        "deepseek-chat",
        WireApi::ChatCompletions,
    )
    .unwrap();

    assert!(body.get("cache_control").is_none());
}

#[test]
fn minimax_openai_upstreams_ask_for_reasoning_split() {
    let preset = crate::providers::PRESETS
        .iter()
        .find(|p| p.id == "opencode-zen")
        .expect("opencode-zen preset");
    let provider = Provider::from_preset(preset);

    let responses_payload = json!({"input": [], "stream": false});
    let (_, responses_body, _) = build_upstream(
        &provider,
        &responses_payload,
        "minimax-m3",
        WireApi::Responses,
    )
    .unwrap();
    assert_eq!(responses_body["reasoning_split"], true);

    let chat_payload = json!({"messages": [], "stream": false});
    let (_, chat_body, _) = build_upstream(
        &provider,
        &chat_payload,
        "minimax-m3",
        WireApi::ChatCompletions,
    )
    .unwrap();
    assert_eq!(chat_body["reasoning_split"], true);

    let (_, other_body, _) =
        build_upstream(&provider, &responses_payload, "kimi-k3", WireApi::Responses).unwrap();
    assert!(other_body.get("reasoning_split").is_none());
}

#[test]
fn direct_minimax_preset_keeps_reasoning_split_on_official_casing() {
    // The gateways expose MiniMax lowercased ("minimax-m3"); the direct API
    // uses CamelCase ("MiniMax-M3"). The split is what lifts MiniMax's
    // `<think>` blocks out of the content, so a casing regression would show
    // up only as raw thinking text leaking into assistant messages.
    for id in ["minimax", "minimax-cn"] {
        let preset = crate::providers::PRESETS
            .iter()
            .find(|p| p.id == id)
            .unwrap_or_else(|| panic!("{id} preset"));
        let provider = Provider::from_preset(preset);

        for model in preset.default_models.iter().map(|m| m.id) {
            let (path, body, kind) = build_upstream(
                &provider,
                &json!({"input": [], "stream": false}),
                model,
                WireApi::Responses,
            )
            .unwrap();
            assert_eq!(path, "chat/completions", "{id}/{model}");
            assert_eq!(kind, UpstreamKind::OpenAiChat, "{id}/{model}");
            assert_eq!(body["reasoning_split"], true, "{id}/{model}");
        }
    }
}

#[test]
fn opencode_go_deepseek_adapts_custom_tools_for_responses() {
    let provider = multi_dialect_provider();
    let payload = json!({
        "input": [
            {"type": "message", "id": "msg_previous", "role": "user", "content": "fix it"},
            {"type": "function_call", "id": "fc_previous", "call_id": "call_1", "name": "ping", "arguments": "{}", "internal_chat_message_metadata_passthrough": {"secret": true}},
            {"type": "function_call_output", "id": "fco_previous", "call_id": "call_1", "output": [{"type": "input_text", "text": "first"}, {"type": "output_text", "text": "second"}], "internal_chat_message_metadata_passthrough": {"secret": true}}
        ],
        "stream": true,
        "generate": true,
        "tools": [{
            "type": "custom",
            "name": "apply_patch",
            "description": "Apply a patch",
            "format": {"type": "grammar", "syntax": "lark", "definition": "start: \"ok\""}
        }]
    });

    let (path, body, kind) =
        build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

    assert_eq!(path, "responses");
    assert_eq!(kind, UpstreamKind::Responses);
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "apply_patch");
    assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    assert_eq!(
        body["tools"][0]["parameters"]["properties"]["input"]["type"],
        "string"
    );
    assert!(body["tools"][0].get("format").is_none());
    assert!(body.get("generate").is_none());
    assert!(body["input"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item.get("id").is_none()));
    assert_eq!(body["input"][1]["call_id"], "call_1");
    assert_eq!(body["input"][2]["call_id"], "call_1");
    assert_eq!(body["input"][2]["output"], "first\nsecond");
    assert!(body["input"].as_array().unwrap().iter().all(|item| item
        .get("internal_chat_message_metadata_passthrough")
        .is_none()));
}

#[test]
fn ws_translator_restores_freeform_tools_for_compat_responses_upstream() {
    let provider = multi_dialect_provider();
    let payload = json!({"tools": [{"type":"custom","name":"apply_patch","description":"p"}]});

    let compat = ws_translator_config(
        &provider,
        "deepseek-v4-flash",
        "opencode-go/deepseek-v4-flash",
        UpstreamKind::Responses,
        &payload,
    )
    .unwrap();
    assert_eq!(compat.0, UpstreamKind::Responses);
    assert_eq!(compat.1, "opencode-go/deepseek-v4-flash");
    assert!(compat.3.contains("apply_patch"));

    assert!(ws_translator_config(
        &provider,
        "gpt-5.6-luna",
        "gpt-5.6-luna",
        UpstreamKind::Responses,
        &payload,
    )
    .is_none());
}

#[test]
fn opencode_go_deepseek_normalizes_an_empty_responses_input() {
    let provider = multi_dialect_provider();
    let payload = json!({
        "input": [],
        "instructions": "prewarm",
        "stream": true
    });

    let (_, body, _) =
        build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

    assert_eq!(body["input"], "");
    assert_eq!(body["instructions"], "prewarm");
}

#[test]
fn opencode_go_deepseek_normalizes_input_after_dropping_every_orphan_output() {
    let provider = multi_dialect_provider();
    let payload = json!({
        "input": [
            {"type": "function_call_output", "call_id": "orphan-output", "output": "result"}
        ],
        "instructions": "continue",
        "stream": true
    });

    let (_, body, _) =
        build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

    assert_eq!(body["input"], "");
    assert_eq!(body["instructions"], "continue");
}

#[test]
fn opencode_go_deepseek_flattens_agent_messages_before_sending_responses() {
    let provider = multi_dialect_provider();
    let payload = json!({
        "input": [{
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/child",
            "content": [
                {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/child\nSender: /root\nPayload:\n"},
                {"type":"encrypted_content","encrypted_content":"Analyze the frontend."}
            ]
        }],
        "stream": true,
        "tools": []
    });

    let (_, body, _) =
        build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

    let item = &body["input"][0];
    assert_eq!(item["type"], "message");
    assert_eq!(item["role"], "user");
    assert_eq!(item["content"][0]["type"], "input_text");
    assert_eq!(item["content"][1]["type"], "input_text");
    assert_eq!(item["content"][1]["text"], "Analyze the frontend.");
}

#[test]
fn opencode_go_flattens_encrypted_content_before_chat_completions() {
    let provider = multi_dialect_provider();
    let payload = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type":"text","text":"Task:\n"},
                {"type":"encrypted_content","encrypted_content":"Review the change."}
            ]
        }],
        "stream": false
    });

    let (_, body, kind) =
        build_upstream(&provider, &payload, "kimi-k3", WireApi::ChatCompletions).unwrap();

    assert_eq!(kind, UpstreamKind::OpenAiChat);
    let content = body["messages"][0]["content"].as_array().unwrap();
    assert!(content
        .iter()
        .all(|part| part.get("type").and_then(Value::as_str) != Some("encrypted_content")));
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "Review the change.");
}

#[test]
fn opencode_go_deepseek_groups_interleaved_calls_before_outputs() {
    let provider = multi_dialect_provider();
    let payload = json!({
        "input": [
            {"type": "message", "role": "user", "content": "run both"},
            {"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": "plan"}]},
            {"type": "function_call", "call_id": "call_1", "name": "first", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "first result"},
            {"type": "function_call", "call_id": "call_2", "name": "second", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_2", "output": "second result"}
        ],
        "stream": true,
        "tools": []
    });

    let (_, body, _) =
        build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

    let item_types = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        item_types,
        vec![
            "message",
            "reasoning",
            "function_call",
            "function_call",
            "function_call_output",
            "function_call_output"
        ]
    );
    assert_eq!(body["input"][2]["call_id"], "call_1");
    assert_eq!(body["input"][3]["call_id"], "call_2");
    assert_eq!(body["input"][4]["call_id"], "call_1");
    assert_eq!(body["input"][5]["call_id"], "call_2");
}

#[test]
fn opencode_go_deepseek_drops_orphan_tool_output() {
    let provider = multi_dialect_provider();
    let payload = json!({
        "input": [
            {"type": "message", "role": "user", "content": "continue"},
            {"type": "function_call_output", "call_id": "orphan-output", "output": "result"}
        ],
        "stream": true,
        "tools": []
    });

    let (_, body, _) =
        build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

    let items = body["input"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["type"], "message");
}

#[test]
fn opencode_go_deepseek_moves_interleaved_assistant_message_after_tool_output() {
    let provider = multi_dialect_provider();
    let payload = json!({
        "input": [
            {"type": "message", "role": "user", "content": "inspect it"},
            {"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": "plan"}]},
            {"type": "function_call", "call_id": "call_1", "name": "inspect", "arguments": "{}"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "checking"}]},
            {"type": "function_call_output", "call_id": "call_1", "output": "result"}
        ],
        "stream": true,
        "tools": []
    });

    let (_, body, _) =
        build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

    let items = body["input"].as_array().unwrap();
    let item_types = items
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        item_types,
        vec![
            "message",
            "reasoning",
            "function_call",
            "function_call_output",
            "message"
        ]
    );
    assert_eq!(items[2]["call_id"], "call_1");
    assert_eq!(items[3]["call_id"], "call_1");
    assert_eq!(items[4]["role"], "assistant");
}

#[test]
fn opencode_go_deepseek_moves_interleaved_developer_message_after_tool_output() {
    let provider = multi_dialect_provider();
    let payload = json!({
        "input": [
            {"type": "message", "role": "user", "content": "inspect it"},
            {"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": "plan"}]},
            {"type": "function_call", "call_id": "call_1", "name": "inspect", "arguments": "{}"},
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "context update"}]},
            {"type": "function_call_output", "call_id": "call_1", "output": "result"}
        ],
        "stream": true,
        "tools": []
    });

    let (_, body, _) =
        build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

    let items = body["input"].as_array().unwrap();
    assert_eq!(items[2]["type"], "function_call");
    assert_eq!(items[3]["type"], "function_call_output");
    assert_eq!(items[4]["type"], "message");
    assert_eq!(items[4]["role"], "developer");
}

#[test]
fn opencode_go_deepseek_replays_summary_only_reasoning_as_reasoning_text() {
    let provider = multi_dialect_provider();
    let payload = json!({
        "input": [
            {"type": "message", "role": "user", "content": "run the tool"},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "plan"}]},
            {"type": "function_call", "call_id": "call_1", "name": "ping", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ],
        "stream": true
    });

    let (_, body, _) =
        build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

    let reasoning = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap();
    assert_eq!(reasoning["content"][0]["type"], "reasoning_text");
    assert_eq!(reasoning["content"][0]["text"], "plan");
}

/// A Responses payload carrying Codex's turn-metadata marker, exactly as
/// codex-rs emits it: client_metadata["x-codex-turn-metadata"] is a JSON
/// string with a `request_kind` field.
fn payload_with_kind(kind: &str) -> Value {
    json!({
        "model": "gpt-5.5",
        "input": [],
        "stream": true,
        "client_metadata": {
            "x-codex-turn-metadata": json!({"request_kind": kind}).to_string(),
        },
    })
}

fn headers_with_kind(kind: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-codex-turn-metadata",
        json!({"request_kind": kind})
            .to_string()
            .parse()
            .expect("header value"),
    );
    headers
}

#[test]
fn auxiliary_kinds_are_side_calls() {
    for kind in ["compaction", "prewarm", "memory"] {
        assert!(
            is_side_call(&payload_with_kind(kind), None),
            "request_kind {kind} must be detected as a side call"
        );
    }
}

#[test]
fn main_turn_is_never_a_side_call() {
    // Explicit main-turn marker.
    assert!(!is_side_call(&payload_with_kind("turn"), None));
    // No metadata at all (older Codex versions, third-party clients).
    assert!(!is_side_call(
        &json!({"model": "gpt-5.5", "input": []}),
        None
    ));
    // client_metadata without the Codex turn marker.
    assert!(!is_side_call(
        &json!({"model": "m", "client_metadata": {"session_id": "s"}}),
        None
    ));
    // Marker present but not valid JSON.
    assert!(!is_side_call(
        &json!({"model": "m", "client_metadata": {"x-codex-turn-metadata": "not json"}}),
        None
    ));
    // Marker JSON without request_kind.
    assert!(!is_side_call(
        &json!({"model": "m", "client_metadata": {
            "x-codex-turn-metadata": json!({"session_id": "s"}).to_string()
        }}),
        None
    ));
}

#[test]
fn header_marker_is_detected() {
    let payload = json!({"model": "gpt-5.5", "input": []});
    assert!(is_side_call(
        &payload,
        Some(&headers_with_kind("compaction"))
    ));
    assert!(is_side_call(&payload, Some(&headers_with_kind("prewarm"))));
    assert!(!is_side_call(&payload, Some(&headers_with_kind("turn"))));
    // Body marker wins when both are present.
    assert!(is_side_call(
        &payload_with_kind("compaction"),
        Some(&headers_with_kind("turn"))
    ));
}

#[test]
fn bare_native_model_is_not_captured_in_normal_mode() {
    let mut cfg = demo_config(None);
    cfg.providers.get_mut("cheap").unwrap().models[0].id = "gpt-5.5".into();

    assert!(resolve(&cfg, "gpt-5.5").is_err());
    assert!(matches!(
        resolve_effective(&cfg, "gpt-5.5", &json!({"model": "gpt-5.5"}), None),
        EffectiveRoute::Native
    ));
}

#[test]
fn qualified_model_routes_despite_a_native_name_collision() {
    let mut cfg = demo_config(None);
    cfg.providers.get_mut("cheap").unwrap().models[0].id = "gpt-5.5".into();

    let (provider, upstream) = resolve(&cfg, "cheap/gpt-5.5").unwrap();
    assert_eq!(provider.id, "cheap");
    assert_eq!(upstream, "gpt-5.5");
}

#[test]
fn bare_model_routes_when_native_slug_mode_is_enabled() {
    let mut cfg = demo_config(None);
    cfg.native_slug_mode = true;
    cfg.providers.get_mut("cheap").unwrap().models[0].id = "gpt-5.5".into();

    let (provider, upstream) = resolve(&cfg, "gpt-5.5").unwrap();
    assert_eq!(provider.id, "cheap");
    assert_eq!(upstream, "gpt-5.5");
}

#[test]
fn fallback_routes_side_calls() {
    let cfg = demo_config(Some("cheap/mini"));
    // A native-model side call that would otherwise hit the ChatGPT
    // passthrough is rerouted to the fallback provider.
    match resolve_effective(&cfg, "gpt-5.5", &payload_with_kind("compaction"), None) {
        EffectiveRoute::Routed {
            provider,
            upstream_model,
            from_fallback,
        } => {
            assert_eq!(provider.id, "cheap");
            assert_eq!(upstream_model, "mini");
            assert!(from_fallback);
        }
        EffectiveRoute::Native => panic!("side call must take the fallback route"),
    }
    // Header-only marker (WS upgrade / compatibility projection) works too.
    match resolve_effective(
        &cfg,
        "gpt-5.5",
        &json!({"model": "gpt-5.5", "input": []}),
        Some(&headers_with_kind("prewarm")),
    ) {
        EffectiveRoute::Routed { from_fallback, .. } => assert!(from_fallback),
        EffectiveRoute::Native => panic!("header-marked side call must take the fallback"),
    }
}

#[test]
fn route_plan_marks_a_resolved_side_call_fallback_for_retry_policy() {
    // If this flag is lost while dispatch is rearranged, a failed fallback
    // becomes terminal instead of retrying the original destination.
    let config = demo_config(Some("cheap/mini"));
    let plan: RoutePlan = resolve_effective(&config, "gpt-5.5", &payload_with_kind("memory"), None);
    match plan {
        RoutePlan::Routed {
            provider,
            upstream_model,
            from_fallback,
        } => {
            assert_eq!(provider.id, "cheap");
            assert_eq!(upstream_model, "mini");
            assert!(from_fallback);
        }
        RoutePlan::Native => panic!("a resolved side-call fallback must be routed"),
    }
}

#[test]
fn fallback_never_touches_main_turns() {
    let cfg = demo_config(Some("cheap/mini"));
    // Native model, main turn: unchanged native passthrough.
    assert!(matches!(
        resolve_effective(&cfg, "gpt-5.5", &payload_with_kind("turn"), None),
        EffectiveRoute::Native
    ));
    assert!(matches!(
        resolve_effective(
            &cfg,
            "gpt-5.5",
            &json!({"model": "gpt-5.5", "input": []}),
            None
        ),
        EffectiveRoute::Native
    ));
    // Routed model, main turn: normal routing, not flagged as fallback.
    match resolve_effective(&cfg, "cheap/mini", &payload_with_kind("turn"), None) {
        EffectiveRoute::Routed {
            provider,
            from_fallback,
            ..
        } => {
            assert_eq!(provider.id, "cheap");
            assert!(!from_fallback);
        }
        EffectiveRoute::Native => panic!("cheap/mini must resolve normally"),
    }
}

#[test]
fn upstream_request_diagnostics_describe_shape_without_request_contents() {
    let body = json!({
        "model": "deepseek-v4-flash",
        "stream": true,
        "reasoning": {"effort": "high"},
        "tools": [
            {
                "type": "function",
                "name": "secret_tool",
                "description": "private tool description",
                "parameters": {"type": "object", "properties": {}}
            },
            {
                "type": "namespace",
                "name": "private_namespace",
                "tools": [{"type": "custom", "name": "secret_nested_tool"}]
            }
        ],
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "top-secret prompt"},
                    {"type": "input_text", "text": "<visual-evidence>private image analysis</visual-evidence>"}
                ]
            },
            {"type": "reasoning", "summary": []}
        ]
    });

    let diagnostics = upstream_request_diagnostics(&body);

    assert_eq!(diagnostics.message_count, 2);
    assert_eq!(diagnostics.tool_count, 2);
    assert_eq!(diagnostics.input_item_types["message"], 1);
    assert_eq!(diagnostics.input_item_types["reasoning"], 1);
    assert_eq!(diagnostics.tool_types["function"], 1);
    assert_eq!(diagnostics.tool_types["namespace"], 1);
    assert_eq!(diagnostics.nested_tool_types["custom"], 1);
    assert_eq!(diagnostics.function_parameter_root_types["object"], 1);
    assert!(diagnostics.top_level_fields.contains("reasoning"));
    assert!(diagnostics.has_visual_evidence);
    assert!(diagnostics.has_reasoning_effort);
    let rendered = format!("{diagnostics:?}");
    assert!(!rendered.contains("top-secret prompt"));
    assert!(!rendered.contains("private image analysis"));
    assert!(!rendered.contains("secret_tool"));
    assert!(!rendered.contains("secret_nested_tool"));
    assert!(!rendered.contains("private_namespace"));
    assert!(!rendered.contains("private tool description"));
}

#[test]
fn upstream_request_diagnostics_count_tool_call_pairing_without_exposing_ids() {
    let body = json!({
        "input": [
            {"type": "function_call", "call_id": "matched-secret", "name": "first"},
            {"type": "function_call_output", "call_id": "matched-secret", "output": "private output"},
            {"type": "function_call", "call_id": "orphan-call-secret", "name": "second"},
            {"type": "function_call_output", "call_id": "orphan-output-secret", "output": "private output"},
            {"type": "function_call_output", "call_id": "out-of-order-secret", "output": "private output"},
            {"type": "function_call", "call_id": "out-of-order-secret", "name": "third"}
        ]
    });

    let diagnostics = upstream_request_diagnostics(&body);

    assert_eq!(diagnostics.matched_function_call_count, 2);
    assert_eq!(diagnostics.unmatched_function_call_count, 1);
    assert_eq!(diagnostics.unmatched_function_output_count, 1);
    assert_eq!(diagnostics.function_output_before_call_count, 1);
    assert_eq!(diagnostics.function_output_value_types["string"], 3);
    assert_eq!(diagnostics.function_call_field_sets["call_id,name,type"], 3);
    assert_eq!(
        diagnostics.function_output_field_sets["call_id,output,type"],
        3
    );
    let rendered = format!("{diagnostics:?}");
    for secret in [
        "matched-secret",
        "orphan-call-secret",
        "orphan-output-secret",
        "out-of-order-secret",
        "private output",
    ] {
        assert!(!rendered.contains(secret));
    }
}

#[test]
fn upstream_request_diagnostics_describe_reasoning_shape_without_exposing_text() {
    let body = json!({
        "input": [
            {"type": "message", "role": "user", "content": "question"},
            {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "private summary"}],
                "content": [
                    {"type": "reasoning_text", "text": "private reasoning"},
                    {"type": "text", "text": "private auxiliary text"}
                ],
                "encrypted_content": "private encrypted reasoning"
            },
            {"type": "function_call", "call_id": "private-call", "name": "tool"}
        ]
    });

    let diagnostics = upstream_request_diagnostics(&body);

    assert_eq!(diagnostics.reasoning_positions, vec![1]);
    assert_eq!(
        diagnostics.reasoning_field_sets["content,encrypted_content,summary,type"],
        1
    );
    assert_eq!(
        diagnostics.reasoning_content_part_types["reasoning_text"],
        1
    );
    assert_eq!(diagnostics.reasoning_content_part_types["text"], 1);
    assert_eq!(diagnostics.reasoning_content_text_bytes, 39);
    assert_eq!(diagnostics.reasoning_summary_part_types["summary_text"], 1);
    assert_eq!(diagnostics.reasoning_summary_text_bytes, 15);
    assert_eq!(diagnostics.reasoning_encrypted_content_count, 1);
    let rendered = format!("{diagnostics:?}");
    for secret in [
        "private summary",
        "private reasoning",
        "private auxiliary text",
        "private encrypted reasoning",
        "private-call",
    ] {
        assert!(!rendered.contains(secret));
    }
}

#[test]
fn disabled_fallback_leaves_routing_unchanged() {
    let cfg = demo_config(None);
    // Side call on a native model: still the native passthrough.
    assert!(matches!(
        resolve_effective(&cfg, "gpt-5.5", &payload_with_kind("compaction"), None),
        EffectiveRoute::Native
    ));
    // Side call on a routed model: normal routing, no fallback flag.
    match resolve_effective(&cfg, "cheap/mini", &payload_with_kind("compaction"), None) {
        EffectiveRoute::Routed { from_fallback, .. } => assert!(!from_fallback),
        EffectiveRoute::Native => panic!("cheap/mini must resolve normally"),
    }
}

#[test]
fn unknown_or_disabled_fallback_slug_is_ignored() {
    // Unknown provider in the slug.
    let cfg = demo_config(Some("nope/missing"));
    assert!(matches!(
        resolve_effective(&cfg, "gpt-5.5", &payload_with_kind("compaction"), None),
        EffectiveRoute::Native
    ));
    // Known provider but disabled.
    let mut cfg = demo_config(Some("cheap/mini"));
    cfg.providers.get_mut("cheap").unwrap().enabled = false;
    assert!(matches!(
        resolve_effective(&cfg, "gpt-5.5", &payload_with_kind("compaction"), None),
        EffectiveRoute::Native
    ));
}

#[test]
fn claude_responses_tool_images_select_structured_cli_input() {
    let payload = serde_json::json!({
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"inspect"}]},
            {"type":"function_call","call_id":"view_image:1","name":"view_image","arguments":"{}"},
            {"type":"function_call_output","call_id":"view_image:1","output":[
                {"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}
            ]}
        ]
    });

    let (input, _) = claude_turn_input(&payload, "claude-opus-5", WireApi::Responses).unwrap();
    let crate::claude_cli::ClaudeTurnInput::StreamJson(rendered) = input else {
        panic!("a tool image must select Claude's structured input path");
    };
    assert!(rendered.contains(r#""type":"image""#), "{rendered}");
    assert!(rendered.contains(r#""data":"aGVsbG8=""#), "{rendered}");
}

#[test]
fn claude_chat_remote_images_select_structured_cli_input() {
    let payload = serde_json::json!({
        "messages": [{"role":"user","content":[
            {"type":"text","text":"inspect"},
            {"type":"image_url","image_url":{"url":"https://example.test/image.png"}}
        ]}]
    });

    let (input, _) =
        claude_turn_input(&payload, "claude-opus-5", WireApi::ChatCompletions).unwrap();
    let crate::claude_cli::ClaudeTurnInput::StreamJson(rendered) = input else {
        panic!("a chat image must select Claude's structured input path");
    };
    assert!(rendered.contains("https://example.test/image.png"));
}

#[test]
fn native_image_target_strips_v1_and_preserves_native_image_paths() {
    assert_eq!(
        native_image_target(
            Some("/images/generations"),
            "https://chatgpt.com/backend-api/codex"
        ),
        Some("https://chatgpt.com/backend-api/codex/images/generations".to_string())
    );
    assert_eq!(
        native_image_target(
            Some("/v1/images/edits"),
            "https://chatgpt.com/backend-api/codex"
        ),
        Some("https://chatgpt.com/backend-api/codex/images/edits".to_string())
    );
    assert_eq!(
        native_image_target(
            Some("/v1/responses"),
            "https://chatgpt.com/backend-api/codex"
        ),
        None
    );
}

#[test]
fn family_resolution_prefers_preset_metadata_over_url_heuristics() {
    let mut provider = multi_dialect_provider();
    provider.id = "deepseek".to_string();
    provider.base_url = "https://custom-proxy.invalid/v1".to_string();

    assert_eq!(family_of(&provider), ProviderFamily::DeepSeek);
}

#[test]
fn family_resolution_falls_back_for_custom_endpoints() {
    let mut provider = multi_dialect_provider();
    provider.id = "custom-endpoint".to_string();
    provider.base_url = "https://api.moonshot.example/v1".to_string();

    assert_eq!(family_of(&provider), ProviderFamily::Kimi);
}
