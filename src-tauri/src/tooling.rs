//! Detection and consent-based import helpers for local coding tools.

use crate::{
    claude_cli::ClaudeAuthStatus,
    config::{AppConfig, Provider},
    providers::{Preset, PRESETS},
};
use anyhow::{anyhow, Context};
use serde::Serialize;
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

const OPENCODE_IDS: [&str; 2] = ["opencode-zen", "opencode-go"];
const CLAUDE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolDetection {
    pub claude: ClaudeDetection,
    pub opencode: OpenCodeDetection,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ClaudeDetection {
    pub detected: bool,
    pub logged_in: Option<bool>,
    pub already_imported: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OpenCodeDetection {
    pub config_found: bool,
    pub gateways: Vec<GatewayDetection>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GatewayDetection {
    pub id: String,
    pub name: String,
    pub importable: bool,
    pub already_imported: bool,
}

struct Gateway {
    preset: &'static Preset,
    key: Option<String>,
}

pub fn opencode_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("opencode").join("opencode.json"))
}

pub fn is_opencode_gateway(id: &str) -> bool {
    OPENCODE_IDS.contains(&id)
}

pub async fn detect_tools(config: &AppConfig, path: PathBuf) -> ToolDetection {
    let imported = config.providers.keys().cloned().collect::<Vec<_>>();
    let opencode = tokio::task::spawn_blocking(move || detect_opencode(&path, &imported))
        .await
        .unwrap_or_else(|_| OpenCodeDetection {
            config_found: false,
            gateways: Vec::new(),
        });
    let claude = detect_claude(
        config
            .providers
            .contains_key(crate::providers::CLAUDE_CODE_PROVIDER_ID),
    )
    .await;
    ToolDetection { claude, opencode }
}

pub(crate) async fn detect_claude(already_imported: bool) -> ClaudeDetection {
    let binary = tokio::task::spawn_blocking(crate::claude_cli::claude_binary)
        .await
        .ok()
        .flatten();
    if binary.is_none() {
        return claude_detection(false, None, already_imported);
    }
    let status = tokio::time::timeout(CLAUDE_PROBE_TIMEOUT, crate::claude_cli::auth_status())
        .await
        .ok();
    claude_detection(true, status, already_imported)
}

fn claude_detection(
    detected: bool,
    status: Option<ClaudeAuthStatus>,
    already_imported: bool,
) -> ClaudeDetection {
    let logged_in = status.and_then(|status| {
        if status.error.is_none() || status.error.as_deref() == Some("claude CLI is not logged in")
        {
            Some(status.logged_in)
        } else {
            None
        }
    });
    ClaudeDetection {
        detected,
        logged_in,
        already_imported,
    }
}

fn detect_opencode(path: &Path, imported: &[String]) -> OpenCodeDetection {
    let Ok(gateways) = read_gateways(path) else {
        return OpenCodeDetection {
            config_found: false,
            gateways: Vec::new(),
        };
    };
    OpenCodeDetection {
        config_found: true,
        gateways: OPENCODE_IDS
            .iter()
            .filter_map(|id| gateways.get(*id))
            .map(|gateway| GatewayDetection {
                id: gateway.preset.id.to_string(),
                name: gateway.preset.name.to_string(),
                importable: gateway.key.is_some(),
                already_imported: imported.iter().any(|id| id == gateway.preset.id),
            })
            .collect(),
    }
}

pub fn provider_from_opencode(path: &Path, gateway_id: &str) -> anyhow::Result<Provider> {
    let preset = opencode_preset(gateway_id)
        .ok_or_else(|| anyhow!("unknown OpenCode gateway '{gateway_id}'"))?;
    let gateway = read_gateways(path)?
        .remove(gateway_id)
        .ok_or_else(|| anyhow!("OpenCode gateway '{gateway_id}' is no longer available"))?;
    let key = gateway
        .key
        .ok_or_else(|| anyhow!("no reusable key found for '{gateway_id}'"))?;
    let mut provider = Provider::from_preset(preset);
    provider.api_key = Some(key);
    Ok(provider)
}

pub fn claude_provider() -> Provider {
    Provider::from_preset(
        PRESETS
            .iter()
            .find(|preset| preset.id == crate::providers::CLAUDE_CODE_PROVIDER_ID)
            .expect("claude-code preset"),
    )
}

fn read_gateways(path: &Path) -> anyhow::Result<BTreeMap<String, Gateway>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("could not read OpenCode config at {}", path.display()))?;
    let value: Value = json5::from_str(&raw).context("could not parse OpenCode config")?;
    let mut gateways = BTreeMap::new();
    collect_gateways(&value, &mut gateways);
    Ok(gateways)
}

fn collect_gateways(value: &Value, gateways: &mut BTreeMap<String, Gateway>) {
    match value {
        Value::Object(object) => {
            if let Some(base_url) = string_field(object, &["baseURL", "baseUrl", "base_url"]) {
                if let Some(preset) = preset_for_url(base_url) {
                    let key = key_field(object).and_then(resolve_key);
                    gateways
                        .entry(preset.id.to_string())
                        .and_modify(|gateway| {
                            if gateway.key.is_none() {
                                gateway.key = key.clone();
                            }
                        })
                        .or_insert(Gateway { preset, key });
                }
            }
            for child in object.values() {
                collect_gateways(child, gateways);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_gateways(child, gateways);
            }
        }
        _ => {}
    }
}

fn string_field<'a>(object: &'a Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| object.get(*name)?.as_str())
}

fn key_field(object: &Map<String, Value>) -> Option<&str> {
    string_field(object, &["apiKey", "api_key", "key"]).or_else(|| {
        object
            .get("options")?
            .as_object()
            .and_then(|options| string_field(options, &["apiKey", "api_key", "key"]))
    })
}

fn resolve_key(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if let Some(variable) = raw
        .strip_prefix("{env:")
        .and_then(|value| value.strip_suffix('}'))
    {
        return std::env::var(variable.trim())
            .ok()
            .filter(|value| !value.is_empty());
    }
    (!raw.is_empty()).then(|| raw.to_string())
}

fn opencode_preset(id: &str) -> Option<&'static Preset> {
    is_opencode_gateway(id)
        .then(|| PRESETS.iter().find(|preset| preset.id == id))
        .flatten()
}

fn preset_for_url(url: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| {
        OPENCODE_IDS.contains(&preset.id)
            && preset.base_url.trim_end_matches('/') == url.trim_end_matches('/')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture(raw: &str) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        std::fs::write(&path, raw).unwrap();
        (dir, path)
    }

    fn zen(key: &str) -> String {
        r#"{"provider":{"zen":{"options":{"baseURL":"https://opencode.ai/zen/v1","apiKey":KEY}}}}"#
            .replace("KEY", key)
    }

    #[test]
    fn ut_011_zen_is_detected_as_importable() {
        let (_dir, path) = fixture(&zen(r#""secret""#));
        let found = detect_opencode(&path, &[]);
        assert!(found.config_found);
        assert_eq!(found.gateways.len(), 1);
        assert!(found.gateways[0].importable);
    }

    #[test]
    fn ut_012_and_ut_018_both_gateways_are_independent() {
        let (_dir, path) = fixture(
            r#"{"provider":{"zen":{"baseURL":"https://opencode.ai/zen/v1","apiKey":"z"},"go":{"baseURL":"https://opencode.ai/zen/go/v1","apiKey":"g"}}}"#,
        );
        let found = detect_opencode(&path, &[]);
        assert_eq!(
            found
                .gateways
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            ["OpenCode Zen", "OpenCode Go"]
        );
    }

    #[test]
    fn ut_015_missing_key_is_not_importable() {
        let (_dir, path) = fixture(&zen("null"));
        assert!(!detect_opencode(&path, &[]).gateways[0].importable);
    }

    #[test]
    fn ut_016_malformed_config_degrades_to_not_found() {
        let (_dir, path) = fixture("not json");
        assert_eq!(
            detect_opencode(&path, &[]),
            OpenCodeDetection {
                config_found: false,
                gateways: vec![]
            }
        );
    }

    #[test]
    fn ut_017_jsonc_is_accepted() {
        let (_dir, path) = fixture(
            r#"{// comment
          "provider":{"zen":{"baseURL":"https://opencode.ai/zen/v1","apiKey":"z",},},}"#,
        );
        assert!(detect_opencode(&path, &[]).gateways[0].importable);
    }

    #[test]
    fn ut_019_missing_canonical_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let found = detect_opencode(&dir.path().join("missing.json"), &[]);
        assert!(!found.config_found);
        assert!(opencode_config_path()
            .unwrap()
            .ends_with(Path::new("opencode/opencode.json")));
    }

    #[test]
    fn ut_020_already_imported_is_per_gateway() {
        let (_dir, path) = fixture(
            r#"{"provider":{"zen":{"baseURL":"https://opencode.ai/zen/v1","apiKey":"z"},"go":{"baseURL":"https://opencode.ai/zen/go/v1","apiKey":"g"}}}"#,
        );
        let found = detect_opencode(&path, &["opencode-zen".into()]);
        assert!(
            found
                .gateways
                .iter()
                .find(|g| g.id == "opencode-zen")
                .unwrap()
                .already_imported
        );
        assert!(
            !found
                .gateways
                .iter()
                .find(|g| g.id == "opencode-go")
                .unwrap()
                .already_imported
        );
    }

    #[test]
    fn ut_021_plain_key_builds_preset_without_leaking_in_detection() {
        let (_dir, path) = fixture(&zen(r#""secret""#));
        let provider = provider_from_opencode(&path, "opencode-zen").unwrap();
        assert_eq!(provider.api_key.as_deref(), Some("secret"));
        assert!(!serde_json::to_string(&detect_opencode(&path, &[]))
            .unwrap()
            .contains("secret"));
    }

    #[test]
    fn ut_022_env_key_resolves() {
        std::env::set_var("LOOM_TEST_OPENCODE_KEY_022", "from-env");
        let (_dir, path) = fixture(&zen(r#""{env:LOOM_TEST_OPENCODE_KEY_022}""#));
        assert_eq!(
            provider_from_opencode(&path, "opencode-zen")
                .unwrap()
                .api_key
                .as_deref(),
            Some("from-env")
        );
        std::env::remove_var("LOOM_TEST_OPENCODE_KEY_022");
    }

    #[test]
    fn ut_023_unknown_gateway_is_rejected() {
        let (_dir, path) = fixture("{}");
        assert!(provider_from_opencode(&path, "custom").is_err());
    }

    #[test]
    fn ut_025_unresolved_env_is_not_importable() {
        std::env::remove_var("LOOM_TEST_MISSING_025");
        let (_dir, path) = fixture(&zen(r#""{env:LOOM_TEST_MISSING_025}""#));
        assert!(!detect_opencode(&path, &[]).gateways[0].importable);
        assert!(provider_from_opencode(&path, "opencode-zen").is_err());
    }

    #[test]
    fn ut_028_stale_config_is_re_read() {
        let (_dir, path) = fixture(&zen(r#""secret""#));
        assert!(detect_opencode(&path, &[]).gateways[0].importable);
        std::fs::write(&path, "{}").unwrap();
        assert!(provider_from_opencode(&path, "opencode-zen").is_err());
    }

    #[test]
    fn ut_029_custom_endpoint_is_ignored() {
        let (_dir, path) = fixture(
            r#"{"provider":{"custom":{"baseURL":"https://example.test/v1","apiKey":"secret"}}}"#,
        );
        assert!(detect_opencode(&path, &[]).gateways.is_empty());
    }

    #[test]
    fn ut_013_ut_030_and_ut_033_map_successful_claude_probes() {
        for logged_in in [false, true] {
            let status = ClaudeAuthStatus {
                logged_in,
                error: (!logged_in).then(|| "claude CLI is not logged in".into()),
                subscription_type: Some("max".into()),
                ..Default::default()
            };
            assert_eq!(
                claude_detection(true, Some(status), false).logged_in,
                Some(logged_in)
            );
        }
    }

    #[test]
    fn ut_014_ut_034_failed_claude_probe_is_unknown() {
        let status = ClaudeAuthStatus {
            error: Some("probe failed".into()),
            ..Default::default()
        };
        assert_eq!(claude_detection(true, Some(status), false).logged_in, None);
        assert_eq!(claude_detection(true, None, false).logged_in, None);
    }

    #[test]
    fn ut_031_missing_claude_is_not_detected() {
        assert!(!claude_detection(false, None, false).detected);
    }

    #[test]
    fn ut_032_and_ut_035_claude_provider_uses_local_login_without_key() {
        let provider = claude_provider();
        assert_eq!(provider.id, "claude-code");
        assert_eq!(provider.base_url, "local");
        assert!(provider.api_key.is_none());
    }
}
