use super::*;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;
#[test]
fn status_flags_orphaned_managed_block() {
    let _guard = codex_home_guard();
    let tmp = std::env::temp_dir().join(format!("loom-codex-status-{}", std::process::id()));
    std::env::set_var("CODEX_HOME", &tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // Orphan: BEGIN without END (the external-rewrite signature).
    std::fs::write(
        tmp.join("config.toml"),
        "# BEGIN loom-router-managed\nmodel_provider = \"loomrouter\"\n",
    )
    .unwrap();
    super::reset_codex_bin_cache();
    catalog::reset_validate_merged_catalog_cache();
    std::env::set_var("CODEX_BIN", "loom-router-test-no-such-codex");
    let status = status(&AppConfig::default());
    assert!(status.managed_block_present);
    assert!(status.managed_block_orphaned);
    assert!(!status.codex_config_loads);
    assert!(status.codex_config_error.is_some());
    std::env::remove_var("CODEX_BIN");
    super::reset_codex_bin_cache();
    catalog::reset_validate_merged_catalog_cache();
    std::env::remove_var("CODEX_HOME");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn status_does_not_flag_complete_managed_block() {
    let _guard = codex_home_guard();
    let tmp = std::env::temp_dir().join(format!("loom-codex-status-ok-{}", std::process::id()));
    std::env::set_var("CODEX_HOME", &tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("config.toml"),
        "# BEGIN loom-router-managed\nmodel_provider = \"loomrouter\"\n# END loom-router-managed\n",
    )
    .unwrap();
    super::reset_codex_bin_cache();
    catalog::reset_validate_merged_catalog_cache();
    std::env::set_var("CODEX_BIN", "loom-router-test-no-such-codex");
    let status = status(&AppConfig::default());
    assert!(status.managed_block_present);
    assert!(!status.managed_block_orphaned);
    assert!(!status.codex_config_loads);
    assert!(status.codex_config_error.is_some());
    std::env::remove_var("CODEX_BIN");
    super::reset_codex_bin_cache();
    catalog::reset_validate_merged_catalog_cache();
    std::env::remove_var("CODEX_HOME");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn status_reads_session_expiry_without_exposing_tokens() {
    let _guard = codex_home_guard();
    let tmp = std::env::temp_dir().join(format!("loom-codex-session-{}", std::process::id()));
    std::env::set_var("CODEX_HOME", &tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let exp = now_ms() / 1_000 + 3_600;
    let payload = URL_SAFE_NO_PAD.encode(json!({"exp": exp}).to_string().as_bytes());
    let token = format!("{header}.{payload}.sig");
    std::fs::write(
        tmp.join("auth.json"),
        json!({"tokens": {"access_token": token, "account_id": "acct_123"}}).to_string(),
    )
    .unwrap();

    let status = codex_session_status(&tmp.join("auth.json"));

    std::env::remove_var("CODEX_HOME");
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(status.present);
    assert!(status.usable);
    assert!(status.has_account_id);
    assert!(!status.expired);
    assert!(status.expires_in_hours.is_some());
    assert!(!status.path.contains(token.as_str()));
}

#[test]
fn status_reports_expired_session_as_unusable() {
    let _guard = codex_home_guard();
    let tmp = std::env::temp_dir().join(format!("loom-codex-expired-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();

    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let exp = now_ms() / 1_000 - 3_600;
    let payload = URL_SAFE_NO_PAD.encode(json!({"exp": exp}).to_string().as_bytes());
    let token = format!("{header}.{payload}.sig");
    std::fs::write(
        tmp.join("auth.json"),
        json!({"tokens": {"access_token": token, "account_id": "acct_123"}}).to_string(),
    )
    .unwrap();

    let status = codex_session_status(&tmp.join("auth.json"));

    let _ = std::fs::remove_dir_all(&tmp);

    assert!(status.present);
    assert!(!status.usable);
    assert!(status.expired);
}
