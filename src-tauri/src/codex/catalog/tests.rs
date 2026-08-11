use super::*;
use crate::config::{Provider, ProviderKey, ProviderModel, ProviderProtocol};
use std::collections::BTreeMap;

fn demo_config() -> AppConfig {
    let mut providers = BTreeMap::new();
    providers.insert(
        "deepseek".into(),
        Provider {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: None,
            keys: vec![ProviderKey {
                id: "deepseek-key".into(),
                name: "Principal".into(),
                enabled: true,
                api_key: Some("demo-key".into()),
                has_key: true,
            }],
            rotation_enabled: false,
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "deepseek-chat".into(),
                label: Some("DeepSeek Chat".into()),
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
        port: 4180,
        providers,
        ..AppConfig::default()
    }
}

#[test]
fn merged_catalog_keeps_native_and_adds_external() {
    let native = json!({"models": [
        {"slug": "gpt-5.5", "display_name": "GPT-5.5", "priority": 1, "visibility": "list",
         "base_instructions": "You are Codex.", "supported_reasoning_levels": ["low","high"]}
    ]});
    let merged = build_merged_catalog(&demo_config(), &native);
    let models = merged["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    // Native entry preserved untouched.
    assert_eq!(models[0]["slug"], "gpt-5.5");
    assert_eq!(
        models[0]["supported_reasoning_levels"],
        json!(["low", "high"])
    );
    // External entry cloned from the template with overrides.
    let ext = &models[1];
    assert_eq!(ext["slug"], "deepseek/deepseek-chat");
    assert_eq!(ext["display_name"], "DeepSeek Chat");
    assert_eq!(ext["visibility"], "list");
    assert_eq!(ext["supported_in_api"], true);
    assert_eq!(ext["base_instructions"], "You are Codex.");
    // DeepSeek is not a Kimi-family provider, so the Kimi name
    // heuristic must NOT apply; without an explicit override the
    // conservative default is published.
    assert_eq!(ext["context_window"], DEFAULT_CONTEXT_WINDOW);
}

#[test]
fn native_catalog_backfills_sol_from_terra_when_the_cli_omits_it() {
    let mut native = json!({"models": [
        {"slug": "gpt-5.6-terra", "display_name": "GPT-5.6-Terra", "priority": 2,
         "visibility": "list", "supported_in_api": true}
    ]});

    ensure_native_catalog_backfills(&mut native);

    let sol = native["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "gpt-5.6-sol")
        .unwrap();
    assert_eq!(sol["display_name"], "GPT-5.6-Sol");
    assert_eq!(sol["priority"], 4);
    assert_eq!(sol["visibility"], "list");
}

#[test]
fn kimi_heuristic_applies_only_to_kimi_family() {
    let mut cfg = demo_config();
    let kimi = crate::providers::PRESETS
        .iter()
        .find(|p| p.id == "kimi-coding")
        .unwrap();
    cfg.providers.insert(
        "kimi-coding".into(),
        crate::config::Provider::from_preset(kimi),
    );
    let merged = build_merged_catalog(&cfg, &json!({"models": []}));
    let models = merged["models"].as_array().unwrap();
    let k3 = models
        .iter()
        .find(|m| m["slug"].as_str() == Some("kimi-coding/k3"))
        .unwrap();
    assert_eq!(k3["context_window"], 1_000_000);
    let k3_256k = models
        .iter()
        .find(|m| m["slug"].as_str() == Some("kimi-coding/k3-256k"))
        .unwrap();
    assert_eq!(k3_256k["context_window"], 262_144);
    // The non-Kimi provider in the same catalog keeps the default.
    let ds = models
        .iter()
        .find(|m| m["slug"].as_str() == Some("deepseek/deepseek-chat"))
        .unwrap();
    assert_eq!(ds["context_window"], DEFAULT_CONTEXT_WINDOW);
}

#[test]
fn context_window_marks_guesses_as_unknown() {
    // The UI shows this value as the model's limit, so a fallback must
    // be distinguishable from a real number — otherwise every provider
    // without an override appears to be a 128k model.
    let kimi = crate::providers::PRESETS
        .iter()
        .find(|p| p.id == "kimi-coding")
        .unwrap();
    let kimi = crate::config::Provider::from_preset(kimi);
    assert_eq!(
        context_window_for(&kimi, "k3"),
        ContextWindow {
            window: 1_000_000,
            known: true
        }
    );
    assert_eq!(
        context_window_for(&kimi, "k3-256k"),
        ContextWindow {
            window: 262_144,
            known: true
        }
    );

    let ds = crate::providers::PRESETS
        .iter()
        .find(|p| p.id == "deepseek")
        .unwrap();
    let mut ds = crate::config::Provider::from_preset(ds);
    assert_eq!(
        context_window_for(&ds, "deepseek-chat"),
        ContextWindow {
            window: DEFAULT_CONTEXT_WINDOW,
            known: false
        },
        "an unconfigured provider must report its window as a guess"
    );

    // An explicit override is a real value, not a guess.
    ds.context_window = Some(64_000);
    assert_eq!(
        context_window_for(&ds, "deepseek-chat"),
        ContextWindow {
            window: 64_000,
            known: true
        }
    );
}

#[test]
fn model_level_context_window_wins_over_provider_and_heuristic() {
    // Discovery fills ProviderModel.context_window from the provider's
    // catalog or models.dev; that real per-model number must beat both
    // the provider-wide override and the Kimi family heuristic.
    let kimi = crate::providers::PRESETS
        .iter()
        .find(|p| p.id == "kimi-coding")
        .unwrap();
    let mut kimi = crate::config::Provider::from_preset(kimi);
    // Heuristic would say 1_000_000 for k3; a discovered 262_144 wins.
    kimi.models
        .iter_mut()
        .find(|m| m.id == "k3")
        .unwrap()
        .context_window = Some(262_144);
    assert_eq!(
        context_window_for(&kimi, "k3"),
        ContextWindow {
            window: 262_144,
            known: true
        }
    );

    let ds = crate::providers::PRESETS
        .iter()
        .find(|p| p.id == "deepseek")
        .unwrap();
    let mut ds = crate::config::Provider::from_preset(ds);
    ds.context_window = Some(64_000);
    // Provider-wide override would say 64_000; a discovered 1M wins.
    ds.models.push(crate::config::ProviderModel {
        id: "deepseek-chat".into(),
        label: None,
        context_window: Some(1_048_576),
        protocol: None,
        fast_mode: false,
        enabled: true,
        supports_vision: false,
    });
    assert_eq!(
        context_window_for(&ds, "deepseek-chat"),
        ContextWindow {
            window: 1_048_576,
            known: true
        }
    );
}

#[test]
fn merged_catalog_works_without_native() {
    let merged = build_merged_catalog(&demo_config(), &json!({"models": []}));
    let models = merged["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["slug"], "deepseek/deepseek-chat");
}

#[test]
fn catalog_advertises_native_vision_without_a_bridge() {
    let mut config = demo_config();
    config.providers.get_mut("deepseek").unwrap().models[0].supports_vision = true;

    let merged = build_merged_catalog(&config, &json!({"models": []}));
    assert_eq!(
        merged["models"][0]["input_modalities"],
        json!(["text", "image"])
    );
}

#[test]
fn catalog_keeps_text_only_model_text_only_when_bridge_is_disabled() {
    let merged = build_merged_catalog(&demo_config(), &json!({"models": []}));
    assert_eq!(merged["models"][0]["input_modalities"], json!(["text"]));
}

#[test]
fn catalog_advertises_images_for_text_only_model_with_valid_bridge() {
    let mut config = demo_config();
    config.providers.get_mut("deepseek").unwrap().api_key = Some("destination-key".into());
    config.providers.insert(
        "vision".into(),
        Provider {
            id: "vision".into(),
            name: "Vision".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: "https://vision.example/v1".into(),
            api_key: Some("assistant-key".into()),
            keys: vec![ProviderKey {
                id: "vision-key".into(),
                name: "Principal".into(),
                enabled: true,
                api_key: Some("assistant-key".into()),
                has_key: true,
            }],
            rotation_enabled: false,
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "vision-model".into(),
                label: None,
                context_window: None,
                protocol: None,
                enabled: true,
                supports_vision: true,
                fast_mode: false,
            }],
            enabled: true,
        },
    );
    config.visual_assistance = crate::config::VisualAssistanceConfig {
        enabled: true,
        assistant_model: Some("vision/vision-model".into()),
        fallback_models: vec![],
    };

    let merged = build_merged_catalog(&config, &json!({"models": []}));
    assert_eq!(
        merged["models"][0]["input_modalities"],
        json!(["text", "image"])
    );
}

#[test]
fn catalog_keeps_text_only_model_text_only_when_bridge_is_invalid() {
    let mut config = demo_config();
    config.visual_assistance = crate::config::VisualAssistanceConfig {
        enabled: true,
        assistant_model: Some("deepseek/deepseek-chat".into()),
        fallback_models: vec![],
    };

    let merged = build_merged_catalog(&config, &json!({"models": []}));
    assert_eq!(merged["models"][0]["input_modalities"], json!(["text"]));
}

// ---------------------------------------------------------------------
// Native slug mode (use Codex without an OpenAI login)
// ---------------------------------------------------------------------

#[test]
fn native_slug_mode_publishes_bare_slugs_and_drops_natives() {
    let mut cfg = demo_config();
    cfg.native_slug_mode = true;
    let native = json!({"models": [
        {"slug": "gpt-5.5", "display_name": "GPT-5.5", "priority": 1, "visibility": "list",
         "base_instructions": "You are Codex.", "supported_reasoning_levels": ["low","high"]}
    ]});
    let merged = build_merged_catalog(&cfg, &native);
    let models = merged["models"].as_array().unwrap();
    // Native GPT entries require the login this mode avoids: dropped.
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["slug"], "deepseek-chat");
    assert_eq!(models[0]["display_name"], "DeepSeek Chat");
    // Metadata still cloned from the native template.
    assert_eq!(models[0]["visibility"], "list");
}

#[test]
fn native_slug_mode_bare_slug_collision_first_provider_wins() {
    let mut cfg = demo_config();
    cfg.native_slug_mode = true;
    // "aaa-other" sorts before "deepseek" in the BTreeMap and serves the
    // same model id, so it must win the bare slug.
    cfg.providers.insert(
        "aaa-other".into(),
        Provider {
            id: "aaa-other".into(),
            name: "Other".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: "https://example.com/v1".into(),
            api_key: None,
            keys: vec![],
            rotation_enabled: false,
            has_key: false,
            context_window: None,
            user_agent: None,
            models: vec![ProviderModel {
                id: "deepseek-chat".into(),
                label: Some("Other Chat".into()),
                context_window: None,
                protocol: None,
                fast_mode: false,
                enabled: true,
                supports_vision: false,
            }],
            enabled: true,
        },
    );
    let merged = build_merged_catalog(&cfg, &json!({"models": []}));
    let models = merged["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["slug"], "deepseek-chat");
    assert_eq!(models[0]["display_name"], "Other Chat");
}
