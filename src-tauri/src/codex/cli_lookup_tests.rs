struct RestoreEnv {
    name: &'static str,
    value: Option<String>,
}

impl RestoreEnv {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            value: std::env::var(name).ok(),
        }
    }
}

impl Drop for RestoreEnv {
    fn drop(&mut self) {
        // SAFETY: every env-mutating test here holds `codex_home_guard`, so no
        // other test can observe the restored value mid-write.
        unsafe {
            match &self.value {
                Some(value) => std::env::set_var(self.name, value.as_str()),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

struct ResetCache;

impl Drop for ResetCache {
    fn drop(&mut self) {
        super::reset_codex_bin_cache();
    }
}

/// Regression: an app launched from Finder inherits launchd's PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), which contains no package-manager
/// bin directory. Probing PATH alone therefore found the CLI when the app
/// was started from a terminal and never when it was double-clicked —
/// and with no CLI there is no native catalog and no merged catalog, so
/// three status rows went red together and the integration looked broken.
///
/// Mutates PATH for the process; every env-mutating test in this module
/// holds the shared `codex_home_guard` so the harness can run them in
/// parallel safely.
#[cfg(unix)]
#[test]
fn resolves_the_cli_under_the_launchd_path() {
    let _guard = super::codex_home_guard();
    let had_cli = super::resolve_codex_bin().is_some();
    if !had_cli {
        eprintln!("no Codex CLI installed here; nothing to resolve");
        return;
    }

    let saved = std::env::var("PATH").ok();
    // SAFETY: single-threaded test, restored below.
    unsafe {
        std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        std::env::remove_var("CODEX_BIN");
    }
    let found = super::resolve_codex_bin();
    unsafe {
        match saved {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }

    assert!(
        found.is_some(),
        "the CLI must still be found without a useful PATH — this is the \
         exact state a Finder-launched app runs in"
    );
}

#[cfg(windows)]
#[test]
fn codex_path_candidates_find_a_native_exe() {
    let root = std::env::temp_dir().join(format!(
        "loom-codex-native-exe-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let native_exe = root.join("codex.exe");
    std::fs::write(&native_exe, b"test").unwrap();
    let path = std::env::join_paths([&root]).unwrap();

    let found = super::path_candidates()
        .iter()
        .find_map(|name| crate::cli_locator::find_in_paths(name, &path));

    assert_eq!(found, Some(native_exe));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn ensure_codex_cli_is_a_noop_when_a_cli_is_already_found() {
    let _guard = super::codex_home_guard();
    let _reset = ResetCache;
    *super::RESOLVED.lock().unwrap_or_else(|p| p.into_inner()) =
        Some(Some("loom-router-test-codex".into()));

    super::ensure_codex_cli().unwrap();
}

#[test]
fn cache_reset_picks_up_a_newly_available_cli() {
    let _guard = super::codex_home_guard();
    let _restore = RestoreEnv::new("CODEX_BIN");
    let _reset = ResetCache;
    unsafe {
        std::env::set_var("CODEX_BIN", "loom-router-test-codex");
    }
    super::reset_codex_bin_cache();

    assert_eq!(
        super::codex_bin().as_deref(),
        Some("loom-router-test-codex")
    );
}
