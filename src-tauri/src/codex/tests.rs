use super::*;
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
    let status = status(&AppConfig::default());
    assert!(status.managed_block_present);
    assert!(status.managed_block_orphaned);
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
    let status = status(&AppConfig::default());
    assert!(status.managed_block_present);
    assert!(!status.managed_block_orphaned);
    std::env::remove_var("CODEX_HOME");
    let _ = std::fs::remove_dir_all(&tmp);
}
