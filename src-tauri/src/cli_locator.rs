use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Walk `PATH` for a binary name, returning the first matching file.
pub(crate) fn find_in_path(bin: &str) -> Option<PathBuf> {
    find_in_paths(bin, &std::env::var_os("PATH")?)
}

pub(crate) fn find_in_paths(bin: &str, path: &OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Whether this command answers `--version` successfully.
///
/// Callers supply CLI-specific environment policy without duplicating the
/// process setup, output suppression, or Windows console behavior.
pub(crate) fn executable_runs<F>(bin: &Path, configure: F) -> bool
where
    F: FnOnce(&mut std::process::Command),
{
    let mut command = std::process::Command::new(bin);
    hide_console_window(&mut command);
    command.arg("--version");
    configure(&mut command);
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

// Production parsing is Unix-only, but portable unit tests exercise it on CI hosts.
#[cfg(any(unix, test))]
fn last_non_empty_line(output: &str) -> Option<&str> {
    output.lines().map(str::trim).rfind(|line| !line.is_empty())
}

/// Resolve a CLI through the user's Unix login shell and validate the path.
///
/// The caller owns the CLI-specific `command -v` expression and validation
/// environment. This helper owns only the shared `-lic` process mechanics,
/// three-second deadline, and banner-safe output parsing.
// Unix shells provide the login-profile PATH recovery used by GUI launches.
#[cfg(unix)]
pub(crate) fn login_shell_lookup<F>(lookup_command: &str, validate: F) -> Option<PathBuf>
where
    F: FnOnce(&Path) -> bool,
{
    login_shell_output(lookup_command).and_then(|output| {
        let path = PathBuf::from(last_non_empty_line(&output)?);
        validate(&path).then_some(path)
    })
}

#[cfg(unix)]
pub(crate) fn login_shell_output(command: &str) -> Option<String> {
    use std::io::Read;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut child = std::process::Command::new(&shell)
        .args(["-lic", command])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!("the login shell did not answer in time; skipping it");
                return None;
            }
            Err(_) => return None,
        }
    }

    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    Some(output)
}

/// Recover the login shell PATH for child processes launched by a GUI app.
#[cfg(unix)]
pub(crate) fn login_shell_path() -> Option<String> {
    login_shell_output("printf '%s' \"$PATH\"").and_then(|output| {
        last_non_empty_line(&output)
            .filter(|path| !path.is_empty())
            .map(str::to_string)
    })
}

// Windows subprocesses need CREATE_NO_WINDOW to avoid flashing a console.
#[cfg(windows)]
pub(crate) fn hide_console_window(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;

    command.creation_flags(0x0800_0000);
}

// Other platforms do not expose or need Windows creation flags.
#[cfg(not(windows))]
pub(crate) fn hide_console_window(_: &mut std::process::Command) {}

/// Env vars whose names indicate credentials or secrets. Matching is case
/// insensitive so Windows's case-insensitive environment cannot bypass it.
fn is_sensitive_env_key(key: &OsStr) -> bool {
    let key = key.to_string_lossy().to_ascii_uppercase();
    ["KEY", "SECRET", "TOKEN", "PASSWORD", "PASSWD", "CREDENTIAL"]
        .iter()
        .any(|marker| key.contains(marker))
}

/// Remove credential-shaped variables from a child std process environment.
///
/// The child still inherits the rest of the environment, so tools that need
/// `HOME`, `PATH`, locale or non-secret config keep working. Explicit
/// `cmd.env` calls made after this helper still win.
pub(crate) fn scrub_child_env_std(command: &mut std::process::Command) {
    for (key, _) in std::env::vars_os() {
        if is_sensitive_env_key(&key) {
            command.env_remove(key);
        }
    }
}

/// Tokio command counterpart to [`scrub_child_env_std`].
pub(crate) fn scrub_child_env_tokio(command: &mut tokio::process::Command) {
    for (key, _) in std::env::vars_os() {
        if is_sensitive_env_key(&key) {
            command.env_remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_scan_returns_the_first_matching_file() {
        let root = std::env::temp_dir().join(format!(
            "loom-cli-locator-path-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let expected = second.join("loom-test-bin");
        std::fs::write(&expected, b"test").unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();

        assert_eq!(find_in_paths("loom-test-bin", &path), Some(expected));

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn bare_name_does_not_match_a_pathex_only_candidate() {
        let root = std::env::temp_dir().join(format!(
            "loom-cli-locator-pathext-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("claude.bat"), b"test").unwrap();
        let path = std::env::join_paths([&root]).unwrap();

        assert_eq!(find_in_paths("claude", &path), None);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn executable_validation_runs_the_version_command() {
        let name = if cfg!(windows) { "rustc.exe" } else { "rustc" };
        let rustc = find_in_path(name).expect("rustc is available while cargo tests run");

        assert!(executable_runs(&rustc, |_| {}));
    }

    #[test]
    fn shell_output_uses_the_last_non_empty_line() {
        assert_eq!(
            last_non_empty_line("startup banner\n\n /usr/local/bin/tool \n"),
            Some("/usr/local/bin/tool")
        );
        assert_eq!(last_non_empty_line("\n \n"), None);
    }
    #[test]
    fn sensitive_env_key_detection_is_case_insensitive() {
        assert!(is_sensitive_env_key(OsStr::new("MY_API_KEY")));
        assert!(is_sensitive_env_key(OsStr::new("a_password")));
        assert!(is_sensitive_env_key(OsStr::new("AUTH_TOKEN")));
        assert!(!is_sensitive_env_key(OsStr::new("HOME")));
        assert!(!is_sensitive_env_key(OsStr::new("LANG")));
    }
}
