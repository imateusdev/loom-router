/// Regression: an app launched from Finder inherits launchd's PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), which contains no package-manager
/// bin directory. Probing PATH alone therefore found the CLI when the app
/// was started from a terminal and never when it was double-clicked —
/// and with no CLI there is no native catalog and no merged catalog, so
/// three status rows went red together and the integration looked broken.
///
/// Mutates PATH for the process, so it is deliberately the only test in
/// this module.
#[cfg(unix)]
#[test]
fn resolves_the_cli_under_the_launchd_path() {
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
