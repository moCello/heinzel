//! The adapters against the real CLIs.
//!
//! Each adapter rests on facts about a CLI heinzel does not own: its flags,
//! its stream, and where it reports the session id. These tests run the real
//! `claude` and `codex` on the built-in config, so they need each CLI and its
//! login, and each run uses the account's usage. They are `#[ignore]`d:
//! `make boundary` runs them, and `make cq` leaves them out.
//!
//! A pass says the adapter's validated version still holds for the CLI on
//! `PATH`. A usage limit is not provoked here. Its classification rests on
//! the unit tests and the stub tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// A scratch home and working directory for one live test.
struct Live {
    root: PathBuf,
}

impl Live {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("heinzel-live-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        // codex refuses to run outside a git repository.
        let git = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.join("work"))
            .status()
            .unwrap();
        assert!(git.success());
        Self { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn heinzel(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_heinzel"))
            .args(args)
            .current_dir(self.work())
            .env("HEINZEL_HOME", self.root.join("home"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "heinzel {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        serde_json::from_str(stdout.lines().last().unwrap_or_default()).unwrap()
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A start that writes the done file ends done and records the session id.
/// A continue of that session resumes it and ends without the done file.
/// `model` is the model to run, or `None` for the CLI's default.
fn start_then_continue(runtime: &str, model: Option<&str>) {
    let live = Live::new(runtime);
    let done = live.work().join("DONE");
    let done_arg = done.display().to_string();
    let message = format!(
        "Create the file {} with the text ok. Do nothing else.",
        done.display()
    );
    let mut start = vec![
        "start",
        "live",
        "--runtime",
        runtime,
        "--done-file",
        &done_arg,
        "--message",
        &message,
    ];
    if let Some(model) = model {
        start.extend(["--model", model]);
    }
    live.heinzel(&start);
    let first = live.heinzel(&["wait", "live"]);
    assert_eq!(first["state"], "done", "{first}");
    assert!(first["session_id"].is_string(), "{first}");
    assert_eq!(first.get("problems"), None, "{first}");
    assert!(Path::new(&done).exists());

    live.heinzel(&[
        "continue",
        "live",
        "--message",
        "Reply with the word ok. Do nothing else.",
    ]);
    let second = live.heinzel(&["wait", "live"]);
    assert_eq!(second["state"], "question", "{second}");
    assert_eq!(second["session_id"], first["session_id"], "{second}");
    assert_eq!(second.get("problems"), None, "{second}");
}

#[test]
#[ignore = "runs the real claude CLI and uses the account's usage"]
fn claude_starts_and_continues() {
    // claude 2.1.285 does not apply `--permission-mode auto` on haiku: its
    // init event says `default`, the write asks for a prompt, and nobody
    // answers it. sonnet applies `auto`. A run in another mode fails the
    // test through the problem it reports.
    start_then_continue("claude", Some("sonnet"));
}

#[test]
#[ignore = "runs the real codex CLI and uses the account's usage"]
fn codex_starts_and_continues() {
    start_then_continue("codex", None);
}
