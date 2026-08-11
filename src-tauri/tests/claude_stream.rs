//! The claude-code bridge must emit frames while the CLI works, not after it
//! finishes.
//!
//! Its own test binary on purpose: `claude_binary()` memoises the resolved
//! path in a `OnceLock`, so `CLAUDE_BIN` only takes effect in a process that
//! has not resolved it yet — and only for the first path it sees. Everything
//! therefore runs as one sequential test against one script whose behaviour is
//! switched through a mode file.

use futures::StreamExt;
use loom_router_lib::claude_cli::{stream_print_turn, ClaudeTurnInput};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct FakeCli {
    dir: PathBuf,
}

impl FakeCli {
    /// A stand-in `claude`. `slow` answers in two instalments with a gap
    /// between them, the way a real agent run interleaves output with tool
    /// work; `fast` does the same without the waits; `fail` exits non-zero.
    fn install() -> Self {
        let dir = std::env::temp_dir().join(format!("loom-claude-stream-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-claude.sh");
        let assistant_one = r#"{"type":"assistant","message":{"usage":{"input_tokens":11},"content":[{"type":"text","text":"first "}]}}"#;
        let assistant_two =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"second"}]}}"#;
        let result = r#"{"type":"result","subtype":"success","result":"first second","session_id":"s1","usage":{"input_tokens":11,"output_tokens":7}}"#;
        let body = format!(
            r#"#!/bin/sh
cat > /dev/null
mode=$(cat "{dir}/mode")
if [ "$mode" = "fail" ]; then
  echo boom >&2
  exit 3
fi
gap=0
[ "$mode" = "slow" ] && gap=1
printf '%s\n' '{assistant_one}'
sleep $gap
printf '%s\n' '{assistant_two}'
sleep $gap
printf '%s\n' '{result}'
"#,
            dir = dir.display(),
        );
        std::fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        std::env::set_var("CLAUDE_BIN", &script);
        Self { dir }
    }

    fn mode(&self, mode: &str) {
        std::fs::write(self.dir.join("mode"), mode).unwrap();
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

/// Run one turn, returning every frame with the moment it arrived.
async fn run_turn() -> (Vec<(Duration, String)>, Option<String>) {
    let started = Instant::now();
    let (mut bytes, failure) = stream_print_turn(
        ClaudeTurnInput::Text("hi".to_string()),
        "claude-opus-5",
        "msg_test",
        None,
    )
    .expect("spawning the fake CLI");

    let mut seen = Vec::new();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.expect("byte stream never yields Err");
        seen.push((
            started.elapsed(),
            String::from_utf8_lossy(&chunk).to_string(),
        ));
    }
    let failed = failure.lock().unwrap().clone();
    (seen, failed)
}

#[tokio::test]
async fn claude_cli_turns_stream_while_they_run() {
    let cli = FakeCli::install();
    assert!(cli.path().is_dir());

    // 1. Frames must arrive DURING the run, not bunched at the end.
    cli.mode("slow");
    let (seen, failed) = run_turn().await;
    assert!(failed.is_none(), "a clean run must not record a failure");

    let first_text = seen
        .iter()
        .find(|(_, frame)| frame.contains("\"text_delta\""))
        .map(|(at, _)| *at)
        .expect("no text delta was ever emitted");
    let last = seen.last().map(|(at, _)| *at).expect("no frames at all");

    // The fake CLI runs for ~2s. Before the fix nothing was emitted until it
    // exited, so the first delta landed together with the last frame.
    assert!(
        first_text < Duration::from_millis(1500),
        "first text delta arrived at {first_text:?}: nothing streamed while the CLI worked"
    );
    assert!(
        last - first_text > Duration::from_millis(500),
        "every frame arrived at once ({first_text:?} -> {last:?}): the run was not streamed"
    );

    // 2. Content and usage must survive the incremental path.
    cli.mode("fast");
    let (seen, failed) = run_turn().await;
    assert!(failed.is_none(), "a clean run must not record a failure");
    let whole: String = seen.into_iter().map(|(_, frame)| frame).collect();

    assert!(whole.contains("first "), "missing the first instalment");
    assert!(whole.contains("second"), "missing the second instalment");
    assert_eq!(
        whole.matches("second").count(),
        1,
        "the trailing result line re-sent text already streamed:\n{whole}"
    );
    assert!(
        whole.contains("\"input_tokens\":11"),
        "prompt tokens must survive into the frames:\n{whole}"
    );
    assert!(
        whole.contains("\"output_tokens\":7"),
        "completion tokens must survive into the frames:\n{whole}"
    );
    assert!(whole.contains("message_stop"), "turn never terminated");

    // 3. A failing CLI must be reported, not swallowed into an empty turn.
    cli.mode("fail");
    let (_, failed) = run_turn().await;
    let failed = failed.expect("a non-zero exit must be reported");
    assert!(
        failed.contains("boom"),
        "the failure must carry the CLI's stderr: {failed}"
    );
}
