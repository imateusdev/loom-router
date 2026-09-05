use super::super::codex_home_guard;
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
            prompt_cache: None,
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
fn strip_only_managed_block() {
    let raw = "model = \"gpt-5\"\n\n# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed\n\n[profiles.work]\n";
    let out = strip_managed_block(raw).unwrap();
    assert!(out.contains("model = \"gpt-5\""));
    assert!(out.contains("[profiles.work]"));
    assert!(!out.contains("openai_base_url"));
}

#[test]
fn strip_hoists_foreign_tables_inside_managed_block() {
    let raw = "model = \"gpt-5\"\n# BEGIN loom-router-managed\nmodel_provider = \"loomrouter\"\nopenai_base_url = \"x\"\n\n[model_providers.loomrouter]\nwire_api = \"responses\"\n\n[marketplaces.openai-bundled]\nlast_updated = \"2026-08-06T13:58:21Z\"\n\n[mcp_servers.loomrouter_subagents]\ncommand = \"x\"\n# END loom-router-managed\n[profiles.work]\n";
    let out = strip_managed_block(raw).unwrap();
    assert!(!out.contains("loomrouter"));
    assert!(!out.contains("wire_api"));
    assert!(out.contains("[marketplaces.openai-bundled]"));
    assert!(out.contains("last_updated = \"2026-08-06T13:58:21Z\""));
    assert!(out.contains("[profiles.work]"));
    toml::from_str::<toml::Value>(&out).unwrap();
}

#[test]
fn strip_refuses_begin_without_end() {
    // An orphan BEGIN with *no* loom-router content is genuinely
    // ambiguous (a stray marker with nothing of ours behind it); the
    // defensive refusal stays so we never guess and delete user data.
    let raw = "model = \"gpt-5\"\n# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n[profiles.work]\n";
    assert!(strip_managed_block(raw).is_err());
}

#[test]
fn orphan_begin_is_recovered_by_ownership() {
    // Replicates a real breakage: the Codex desktop app re-serialized
    // config.toml and dropped the `# END` comment, leaving an orphan
    // BEGIN with our owned root keys + provider table inside it. The
    // strip must remove exactly what is ours and keep the rest.
    let raw = "notify = [\"turn-ended\"]\napproval_policy = \"on-request\"\n# BEGIN loom-router-managed\nmodel_provider = \"loomrouter\"\nopenai_base_url = \"http://127.0.0.1:4180/v1\"\nmodel_catalog_json = \"~/.codex/loom-router/merged-models.json\"\nmodel = \"opencode-zen-chat/deepseek-v4-flash\"\nmodel_reasoning_effort = \"medium\"\n\n[model_providers.loomrouter]\nname = \"OpenAI\"\nbase_url = \"http://127.0.0.1:4180/v1\"\nwire_api = \"responses\"\nhttp_headers = { \"x-loomrouter-token\" = \"t\", \"Authorization\" = \"Bearer t\" }\n\n[marketplaces.openai-bundled]\nlast_updated = \"2026-08-06T13:58:21Z\"\n\n[hooks.state]\n";
    let out = strip_managed_block(raw).unwrap();
    // Our markers and owned root keys are gone.
    assert!(!out.contains(BEGIN_MARK));
    assert!(!out.contains("model_provider"));
    assert!(!out.contains("openai_base_url"));
    assert!(!out.contains("model_catalog_json"));
    assert!(!out.contains("[model_providers.loomrouter]"));
    assert!(!out.contains("x-loomrouter-token"));
    // The user's own settings survive untouched.
    assert!(out.contains("notify = [\"turn-ended\"]"));
    assert!(out.contains("approval_policy = \"on-request\""));
    assert!(out.contains("[marketplaces.openai-bundled]"));
    assert!(out.contains("last_updated = \"2026-08-06T13:58:21Z\""));
    assert!(out.contains("[hooks.state]"));
    // And the result parses cleanly (no duplicate keys left behind).
    let parsed: toml::Value = toml::from_str(&out).unwrap();
    assert_eq!(
        parsed.get("approval_policy").and_then(toml::Value::as_str),
        Some("on-request")
    );
}

#[test]
fn orphan_recovery_removes_only_loomrouter_subagent_mcp() {
    let raw = "# BEGIN loom-router-managed\n\
model_provider = \"loomrouter\"\n\
openai_base_url = \"x\"\n\
model_catalog_json = \"y\"\n\
\n\
[model_providers.loomrouter]\n\
wire_api = \"responses\"\n\
\n\
[mcp_servers.loomrouter_subagents]\n\
command = \"/Applications/LoomRouter\"\n\
args = [\"subagent-mcp\"]\n\
\n\
[mcp_servers.user_owned]\n\
command = \"user-server\"\n";

    let out = strip_managed_block(raw).unwrap();

    assert!(!out.contains("loomrouter_subagents"));
    assert!(out.contains("[mcp_servers.user_owned]"));
    assert!(out.contains("command = \"user-server\""));
}

#[test]
fn orphan_recovery_preserves_model_key_restore_path() {
    // The orphaned region may also swallow root `model`/effort keys the
    // desktop app re-emitted; `remove()` reconciles the root `model`
    // afterwards, so recovery must leave the stripped text parseable.
    let raw = "# BEGIN loom-router-managed\nmodel_provider = \"loomrouter\"\nopenai_base_url = \"x\"\nmodel_catalog_json = \"y\"\nmodel = \"deepseek/deepseek-chat\"\nmodel_reasoning_effort = \"medium\"\n\n[model_providers.loomrouter]\nwire_api = \"responses\"\n\n[profiles.work]\nmodel = \"gpt-5\"\n";
    let out = strip_managed_block(raw).unwrap();
    assert!(!out.contains(BEGIN_MARK));
    assert!(!out.contains("model_provider"));
    assert!(!out.contains("[model_providers.loomrouter]"));
    assert!(out.contains("[profiles.work]"));
    assert!(out.contains("model = \"gpt-5\""));
}

#[test]
fn legacy_unmarked_install_is_stripped() {
    // Pre-marker versions wrote the provider block without BEGIN/END;
    // applying on top duplicated `model_provider` and the parse check
    // refused every write. The legacy shape must migrate away cleanly.
    let raw = "model = \"gpt-5\"\napproval_policy = \"never\"\nmodel_provider = \"loomrouter\"\nopenai_base_url = \"http://127.0.0.1:4180/v1\"\nmodel_catalog_json = \"C:/x/merged-models.json\"\n\n[model_providers.loomrouter]\nname = \"OpenAI\"\nbase_url = \"http://127.0.0.1:4180/v1\"\n\n[model_providers.loomrouter.http_headers]\nAuthorization = \"Bearer t\"\n\n[profiles.work]\nmodel = \"gpt-5\"\nmodel_provider = \"openai\"\n";
    let out = strip_legacy_install(raw);
    // User root keys survive; owned root keys and the provider table go.
    assert!(out.contains("model = \"gpt-5\""));
    assert!(out.contains("approval_policy = \"never\""));
    assert!(!out.contains("openai_base_url"));
    assert!(!out.contains("model_catalog_json"));
    assert!(!out.contains("[model_providers.loomrouter]"));
    assert!(!out.contains("Authorization"));
    // Other tables — including a profile's own `model_provider` — stay.
    assert!(out.contains("[profiles.work]"));
    assert!(out.contains("model_provider = \"openai\""));
    // And a fresh managed block on top parses without duplicate keys.
    let parsed: toml::Value = toml::from_str("").unwrap();
    let out = insert_root_block(
        &out,
        &managed_block(4180, "C:/x/merged-models.json", false, &parsed),
    );
    let parsed: toml::Value = toml::from_str(&out).unwrap();
    assert_eq!(
        parsed.get("model_provider").and_then(toml::Value::as_str),
        Some("loomrouter")
    );
    assert_eq!(
        parsed["profiles"]["work"]["model_provider"].as_str(),
        Some("openai")
    );
}

#[test]
fn legacy_detection_ignores_other_providers() {
    // A user's own provider with no loomrouter table anywhere is not a
    // legacy install: nothing is touched.
    let raw =
        "model_provider = \"openai\"\nopenai_base_url = \"http://example/v1\"\n\n[profiles.work]\n";
    assert_eq!(strip_legacy_install(raw), raw);
}

#[test]
fn crlf_files_keep_crlf() {
    let raw = "model = \"gpt-5\"\r\n\r\n# BEGIN loom-router-managed\r\nopenai_base_url = \"x\"\r\n# END loom-router-managed\r\n\r\n[profiles.work]\r\n";
    let stripped = strip_managed_block(raw).unwrap();
    let block = "# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed";
    let out = insert_root_block(&stripped, block);
    assert!(out.contains("\r\n"));
    // No bare LF line endings were introduced.
    assert!(
        !out.replace("\r\n", "").contains('\n'),
        "bare LF found:\n{out}"
    );
    let parsed: toml::Value = toml::from_str(&out).unwrap();
    assert_eq!(
        parsed.get("openai_base_url").and_then(toml::Value::as_str),
        Some("x")
    );
}

#[test]
fn root_model_key_replaces_only_the_model_key() {
    // `model_provider` and `model_catalog_json` share the prefix and
    // must survive; the user's own `model` is the one that moves.
    let stripped = "model = \"gpt-5.5\"\nmodel_provider = \"loomrouter\"\nmodel_catalog_json = \"/tmp/x.json\"\n\n[profiles.work]\nmodel = \"gpt-5\"\n";
    let out = set_root_model_key(stripped, Some("deepseek/deepseek-chat"));
    assert!(out.contains("model = \"deepseek/deepseek-chat\""));
    assert!(out.contains("model_provider = \"loomrouter\""));
    assert!(out.contains("model_catalog_json = \"/tmp/x.json\""));
    // The profile's own model is below the first table header and is
    // not a root key — it must not be rewritten.
    assert!(out.contains("[profiles.work]\nmodel = \"gpt-5\""));
    let parsed: toml::Value = toml::from_str(&out).unwrap();
    assert_eq!(
        parsed.get("model").and_then(toml::Value::as_str),
        Some("deepseek/deepseek-chat")
    );
}

#[test]
fn root_model_key_inserts_and_clears() {
    let inserted = set_root_model_key("model_provider = \"loomrouter\"\n", Some("a/b"));
    assert!(inserted.starts_with("model = \"a/b\"\n"));
    // Exactly one root `model` key: a duplicate makes Codex reject the
    // whole config.toml.
    assert_eq!(
        inserted
            .lines()
            .filter(|l| l.trim_start().starts_with("model ="))
            .count(),
        1
    );
    let cleared = set_root_model_key(&inserted, None);
    assert!(!cleared.contains("model = \"a/b\""));
    assert!(cleared.contains("model_provider"));
}

#[test]
fn a_users_own_model_is_never_deleted_and_is_given_back() {
    let mut cfg = demo_config();
    let user_config = "model = \"gpt-5.5\"\n[profiles.work]\n";

    // Nothing selected: LoomRouter must not touch a key it does not own.
    assert_eq!(reconcile_root_model(user_config, &cfg), user_config);

    // Selecting a model displaces it (state::set_active_model is what
    // records the backup; here it is already recorded).
    cfg.active_model = Some("deepseek/deepseek-chat".into());
    cfg.codex_model_backup = Some("gpt-5.5".into());
    let switched = reconcile_root_model(user_config, &cfg);
    assert!(switched.contains("model = \"deepseek/deepseek-chat\""));
    assert!(!switched.contains("gpt-5.5"));

    // Clearing the selection restores what Codex had before us.
    cfg.active_model = None;
    let restored = reconcile_root_model(&switched, &cfg);
    assert!(restored.contains("model = \"gpt-5.5\""));
    assert!(!restored.contains("deepseek/deepseek-chat"));
}

#[test]
fn a_stale_slug_of_ours_is_dropped_when_there_is_nothing_to_restore() {
    let mut cfg = demo_config();
    // Ours, but the selection is gone and no backup was ever taken.
    cfg.active_model = None;
    let out = reconcile_root_model("model = \"deepseek/deepseek-chat\"\n", &cfg);
    assert!(!out.contains("model ="), "stale slug survived:\n{out}");
}

#[test]
fn quoted_and_commented_model_keys_are_still_recognized() {
    // A quoted key that went unmatched used to leave the old assignment
    // in place next to the new one — a duplicate key Codex rejects.
    let out = set_root_model_key("\"model\" = \"gpt-5.5\"\n", Some("a/b"));
    assert_eq!(
        out.lines().filter(|l| is_root_model_line(l)).count(),
        1,
        "duplicate root model key:\n{out}"
    );
    toml::from_str::<toml::Value>(&out).unwrap();
    // A trailing comment must not leak into the parsed value.
    assert_eq!(
        root_model_key("model = \"gpt-5.5\" # pinned\n").as_deref(),
        Some("gpt-5.5")
    );
}

#[test]
fn active_slug_follows_native_slug_mode_and_drops_stale_picks() {
    let mut cfg = demo_config();
    cfg.active_model = Some("deepseek/deepseek-chat".into());
    assert_eq!(active_slug(&cfg).as_deref(), Some("deepseek/deepseek-chat"));

    cfg.native_slug_mode = true;
    assert_eq!(active_slug(&cfg).as_deref(), Some("deepseek-chat"));

    // Disabling the model (or its provider) makes the pick unroutable,
    // so nothing is published rather than a dangling pointer.
    cfg.native_slug_mode = false;
    cfg.providers.get_mut("deepseek").unwrap().models[0].enabled = false;
    assert_eq!(active_slug(&cfg), None);
    cfg.providers.get_mut("deepseek").unwrap().models[0].enabled = true;
    cfg.providers.get_mut("deepseek").unwrap().enabled = false;
    assert_eq!(active_slug(&cfg), None);

    cfg.providers.get_mut("deepseek").unwrap().enabled = true;
    cfg.active_model = Some("ghost/model".into());
    assert_eq!(active_slug(&cfg), None);
}

#[test]
fn apply_refuses_empty_catalog_and_rolls_back() {
    // Regression test for the macOS report: applying with zero enabled
    // models wrote an empty merged-models.json, and Codex then refused
    // to load config.toml entirely ("must contain at least one model").
    let _guard = codex_home_guard();
    let tmp = std::env::temp_dir().join(format!("loom-codex-test-{}", std::process::id()));
    std::env::set_var("CODEX_HOME", &tmp);
    // A binary that never runs, so the native capture fails gracefully
    // instead of probing the real CLI.
    std::env::set_var("CODEX_BIN", "loom-router-test-no-such-codex");

    let mut cfg = demo_config();
    for p in cfg.providers.values_mut() {
        p.enabled = false;
    }
    // Seed the broken state: a managed block from a previous apply.
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("config.toml"),
        "model = \"gpt-5.5\"\n\n# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed\n",
    )
    .unwrap();

    let result = apply(&cfg, 4180);

    std::env::remove_var("CODEX_HOME");
    std::env::remove_var("CODEX_BIN");
    let written = std::fs::read_to_string(tmp.join("config.toml")).unwrap();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(result.is_err());
    // The broken managed block was rolled back; user keys survived.
    assert!(!written.contains(BEGIN_MARK));
    assert!(written.contains("model = \"gpt-5.5\""));
}

#[test]
fn patch_state_restores_previous_root_values_after_remove() {
    let _guard = codex_home_guard();
    let tmp = std::env::temp_dir().join(format!("loom-codex-state-{}", std::process::id()));
    std::env::set_var("CODEX_HOME", &tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let previous = "openai_base_url = \"https://example.test/v1\"\nmodel_provider = \"openai\"\n";
    ensure_patch_state(previous).unwrap();
    let restored = restore_patch_state("");

    std::env::remove_var("CODEX_HOME");
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(restored.contains("openai_base_url = \"https://example.test/v1\""));
    assert!(restored.contains("model_provider = \"openai\""));
    toml::from_str::<toml::Value>(&restored).unwrap();
}

#[test]
fn root_block_goes_before_first_table() {
    let raw = "model = \"gpt-5.5\"\n\n[plugins.\"a@b\"]\nenabled = true\n\n[[hooks.SessionStart]]\nmatcher = \"startup\"\n";
    let block = "# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed";
    let out = insert_root_block(raw, block);
    let block_pos = out.find("openai_base_url").unwrap();
    let table_pos = out.find("[plugins").unwrap();
    assert!(
        block_pos < table_pos,
        "block must be in root section:\n{out}"
    );
    assert!(out.contains("model = \"gpt-5.5\""));
    // A TOML parser must see the keys at root level.
    let parsed: toml::Value = toml::from_str(&out).unwrap();
    assert_eq!(
        parsed.get("openai_base_url").and_then(toml::Value::as_str),
        Some("x")
    );
}

#[test]
fn root_block_appends_when_no_tables() {
    let out = insert_root_block("model = \"gpt-5.5\"\n", "# B\nx = 1\n# E");
    assert!(out.contains("model = \"gpt-5.5\"\n\n# B"));
}

#[test]
fn managed_block_is_valid_toml_with_websockets_on() {
    let parsed: toml::Value = toml::from_str("").unwrap();
    let block = managed_block(
        4180,
        "C:/Users/x/.codex/loom-router/merged-models.json",
        false,
        &parsed,
    );
    let out = insert_root_block(
        "model = \"kimi-coding/k3\"\n\n[plugins.a]\nenabled = true\n",
        &block,
    );
    let parsed: toml::Value = toml::from_str(&out).unwrap();
    assert_eq!(
        parsed.get("model_provider").and_then(toml::Value::as_str),
        Some("loomrouter")
    );
    let provider = &parsed["model_providers"]["loomrouter"];
    assert_eq!(provider["supports_websockets"].as_bool(), Some(true));
    assert_eq!(provider["wire_api"].as_str(), Some("responses"));
    assert_eq!(provider["requires_openai_auth"].as_bool(), Some(true));
    // Routed mode has no provider auth command, so no live `/models` refresh
    // worker: the on-disk pointer is the only way Codex sees the catalog.
    assert_eq!(
        parsed
            .get("model_catalog_json")
            .and_then(toml::Value::as_str),
        Some("C:/Users/x/.codex/loom-router/merged-models.json")
    );
    assert!(provider.get("auth").is_none());
    // The block carries the local proxy token so Codex can authenticate.
    let headers = provider["http_headers"].as_table().unwrap();
    assert!(headers.contains_key("x-loomrouter-token"));
    assert!(headers["Authorization"]
        .as_str()
        .unwrap()
        .starts_with("Bearer "));
    let subagents = &parsed["mcp_servers"]["loomrouter_subagents"];
    assert!(subagents["command"].as_str().is_some_and(|v| !v.is_empty()));
    assert_eq!(
        subagents["args"].as_array().unwrap(),
        &[toml::Value::String("subagent-mcp".into())]
    );
    assert_eq!(subagents["tool_timeout_sec"].as_integer(), Some(600));
    assert_eq!(
        subagents["default_tools_approval_mode"].as_str(),
        Some("approve")
    );
    assert_eq!(
        subagents["supports_parallel_tool_calls"].as_bool(),
        Some(true)
    );
    // User tables survive intact after the managed provider table.
    assert_eq!(parsed["plugins"]["a"]["enabled"].as_bool(), Some(true));
    // Stripping removes the whole block, including the provider table.
    let stripped = strip_managed_block(&out).unwrap();
    assert!(!stripped.contains("loomrouter"));
    assert!(stripped.contains("[plugins.a]"));
}

#[test]
fn every_mode_gets_exactly_one_catalog_mechanism() {
    // Codex learns the merged catalog either from the on-disk pointer or from
    // the proxy's `/models`, and the live refresh rides on the provider auth
    // command that only routed mode lacks. Dropping the pointer in both modes
    // once left the default install with no catalog at all.
    let empty: toml::Value = toml::from_str("").unwrap();

    let routed = managed_block(4180, "C:/x/merged-models.json", false, &empty);
    let parsed: toml::Value = toml::from_str(&routed).unwrap();
    assert_eq!(
        parsed
            .get("model_catalog_json")
            .and_then(toml::Value::as_str),
        Some("C:/x/merged-models.json")
    );
    assert!(parsed["model_providers"]["loomrouter"]
        .get("auth")
        .is_none());

    let native = managed_block(4180, "C:/x/merged-models.json", true, &empty);
    let parsed: toml::Value = toml::from_str(&native).unwrap();
    assert!(parsed.get("model_catalog_json").is_none());
    assert!(parsed["model_providers"]["loomrouter"]
        .get("auth")
        .is_some());
}

#[test]
fn native_slug_mode_drops_openai_auth_requirement() {
    let parsed: toml::Value = toml::from_str("").unwrap();
    let block = managed_block(4180, "C:/x/merged-models.json", true, &parsed);
    // BEGIN/END markers are `#` comments, so the block parses as-is.
    let parsed: toml::Value = toml::from_str(&block).unwrap();
    let provider = &parsed["model_providers"]["loomrouter"];
    // The whole point of the mode: no ChatGPT login gate. Codex then
    // authenticates only with the static proxy-token headers.
    assert_eq!(provider["requires_openai_auth"].as_bool(), Some(false));
    let auth = provider["auth"].as_table().unwrap();
    assert_eq!(
        auth["args"].as_array().unwrap(),
        &[toml::Value::String("provider-auth".into())]
    );
    let headers = provider["http_headers"].as_table().unwrap();
    assert!(headers["Authorization"]
        .as_str()
        .unwrap()
        .starts_with("Bearer "));
}

#[test]
fn set_multi_agent_patches_features_without_disturbing_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    let original = "# my config\n\
         model = \"gpt-5.5\"\n\
         \n\
         # BEGIN loom-router-managed\n\
         openai_base_url = \"http://127.0.0.1:4180/v1\"\n\
         # END loom-router-managed\n\
         \n\
         [plugins.a]\n\
         enabled = true\n";
    std::fs::write(&cfg, original).unwrap();

    // Enable: creates the [features] table at the end.
    set_multi_agent_in(&cfg, true).unwrap();
    assert!(multi_agent_enabled_in(&cfg));
    let raw = std::fs::read_to_string(&cfg).unwrap();
    assert!(raw.contains("# my config"));
    assert!(raw.contains(BEGIN_MARK));
    assert!(raw.contains("[plugins.a]"));

    // Both flags are written: multi_agent alone selects Codex's v1
    // surface, where the spawn tool is namespaced and the orchestrator
    // skill's `spawn_agent` does not exist.
    let raw = std::fs::read_to_string(&cfg).unwrap();
    assert!(raw.contains("multi_agent = true"));
    assert!(raw.contains("multi_agent_v2 = true"));

    // Disable: updates the keys in place instead of duplicating them.
    set_multi_agent_in(&cfg, false).unwrap();
    let raw = std::fs::read_to_string(&cfg).unwrap();
    assert!(!multi_agent_enabled_in(&cfg));
    assert_eq!(raw.matches("multi_agent = ").count(), 1);
    assert_eq!(raw.matches("multi_agent_v2 = ").count(), 1);
    assert_eq!(raw.matches("[features]").count(), 1);

    // Enabling again flips the existing key, and an existing
    // [features] table gains the key without moving other content.
    std::fs::write(
        &cfg,
        "[features]\nfoo = 1\n\n[profiles.work]\nmodel = \"gpt-5.5\"\n",
    )
    .unwrap();
    set_multi_agent_in(&cfg, true).unwrap();
    let raw = std::fs::read_to_string(&cfg).unwrap();
    assert!(multi_agent_enabled_in(&cfg));
    assert!(raw.contains("foo = 1"));
    assert!(raw.contains("[profiles.work]"));
    let features_pos = raw.find("multi_agent").unwrap();
    let profiles_pos = raw.find("[profiles.work]").unwrap();
    assert!(features_pos < profiles_pos, "key must live in [features]");
}

#[test]
fn set_multi_agent_rewrites_each_key_not_whatever_shares_its_prefix() {
    // `multi_agent` is a prefix of `multi_agent_v2`. Matching keys with
    // `starts_with` finds whichever appears first and overwrites it, so
    // a file listing v2 first lost the v2 flag on every toggle — the
    // exact flag that decides whether the spawn tool exists at all.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(
        &cfg,
        "[features]\nmulti_agent_v2 = false\nmulti_agent = false\nmemories = true\n",
    )
    .unwrap();

    set_multi_agent_in(&cfg, true).unwrap();
    let raw = std::fs::read_to_string(&cfg).unwrap();

    assert!(multi_agent_enabled_in(&cfg));
    assert!(raw.contains("multi_agent_v2 = true"), "v2 flag: {raw}");
    assert!(raw.contains("multi_agent = true"), "v1 flag: {raw}");
    assert_eq!(raw.matches("multi_agent = ").count(), 1, "no duplicate");
    assert_eq!(raw.matches("multi_agent_v2 = ").count(), 1, "no duplicate");
    assert!(raw.contains("memories = true"), "neighbours untouched");
}

fn managed_block_for_sandbox_fixture(config_toml: &str) -> String {
    let _guard = codex_home_guard();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("CODEX_HOME", dir.path());
    std::fs::write(dir.path().join("config.toml"), config_toml).unwrap();
    let (_, parsed) = load_codex_config();
    let block = managed_block(4180, "C:/x/merged-models.json", false, &parsed);
    std::env::remove_var("CODEX_HOME");
    block
}

#[test]
fn managed_block_includes_danger_full_access_sandbox_env_when_user_uses_dfa() {
    let block = managed_block_for_sandbox_fixture("sandbox_mode = \"danger-full-access\"\n");
    assert!(block.contains("CODEX_PERMISSION_PROFILE = \"danger-full-access\""));
}

#[test]
fn managed_block_includes_workspace_write_sandbox_env_when_user_uses_workspace_write() {
    let block = managed_block_for_sandbox_fixture("sandbox_mode = \"workspace-write\"\n");
    assert!(block.contains("CODEX_PERMISSION_PROFILE = \"workspace-write\""));
}

#[test]
fn managed_block_includes_read_only_sandbox_env_when_user_uses_read_only() {
    let block = managed_block_for_sandbox_fixture("sandbox_mode = \"read-only\"\n");
    assert!(block.contains("CODEX_PERMISSION_PROFILE = \"read-only\""));
}

#[test]
fn managed_block_defaults_to_workspace_write_when_user_has_no_sandbox_mode() {
    let block = managed_block_for_sandbox_fixture("model = \"gpt-5\"\n");
    assert!(block.contains("CODEX_PERMISSION_PROFILE = \"workspace-write\""));
}

#[test]
fn managed_block_invalid_sandbox_mode_string_falls_back_to_workspace_write() {
    let block = managed_block_for_sandbox_fixture("sandbox_mode = \"mystery-mode\"\n");
    assert!(block.contains("CODEX_PERMISSION_PROFILE = \"workspace-write\""));
}
