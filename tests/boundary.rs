//! The adapters against the real CLIs.
//!
//! Each adapter rests on facts about a CLI heinzel does not own: its flags,
//! its stream, and where it reports the session id and the answer. These
//! tests run the real `claude` and `codex`, through the binary on the
//! built-in config and through a library turn. So they need each CLI and its
//! login, and each run uses the account's usage. They are `#[ignore]`d:
//! `make boundary` runs them, and `make cq` leaves them out.
//!
//! One test per runtime runs every scenario for that runtime. When all of
//! them pass, the test writes the version of the CLI on `PATH` to the
//! directory that `$BOUNDARY_RECORD` names, when it names one. After every
//! test it ran passed, `cargo xtask validate` copies that record to
//! `runtime/validated/`. The adapter compiles it in as the version it was
//! validated against. The task runs each test by its name,
//! `the_<runtime>_adapter_holds`.
//!
//! A usage limit is not provoked here. Its classification rests on the unit
//! tests and the stub tests.

use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use heinzel::{Outcome, Runtime, Session, Turn};
use heinzel_runtime::{RuntimeName, installed_version};
use serde_json::Value;

/// A scratch home and working directory for one live test.
struct Live {
    root: PathBuf,
}

impl Live {
    fn new(tag: &str) -> Self {
        let root = env::temp_dir().join(format!("heinzel-live-{tag}-{}", std::process::id()));
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

/// Run `scenarios` on the CLI of `runtime` on `PATH`. Then write its version
/// to `$BOUNDARY_RECORD`, when that names a directory. A CLI whose version
/// changed during the run fails the test.
fn validate(runtime: RuntimeName, scenarios: impl FnOnce()) {
    let name = runtime.as_str();
    let program = Path::new(name);
    let before = installed_version(program).unwrap();
    scenarios();
    let after = installed_version(program).unwrap();
    assert_eq!(after, before, "{name} changed its version during the run");

    if let Some(record) = env::var_os("BOUNDARY_RECORD") {
        let record = PathBuf::from(record);
        fs::create_dir_all(&record).unwrap();
        fs::write(record.join(name), format!("{after}\n")).unwrap();
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

/// A library turn in a new session finishes with an answer. A second turn
/// resumes that session and finishes in it. `turn` is the first turn, and
/// its session names the id the caller chose, if any.
fn turn_then_resume(mut turn: Turn) {
    let chosen = match &turn.session {
        Session::New { id } => id.clone(),
        Session::Resume { .. } => None,
    };
    let first = heinzel::run(&turn).unwrap();
    assert_eq!(first.problems, Vec::<String>::new(), "{first:?}");
    let Outcome::Finished { session_id, answer } = first.outcome else {
        panic!("the first turn did not finish: {first:?}");
    };
    if let Some(chosen) = chosen {
        assert_eq!(session_id, chosen);
    }
    assert!(
        answer.is_some_and(|answer| answer.to_lowercase().contains("ok")),
        "{session_id}"
    );

    turn.session = Session::Resume {
        id: session_id.clone(),
    };
    turn.message = "Reply with the word yes. Do nothing else.".to_string();
    let second = heinzel::run(&turn).unwrap();
    assert_eq!(second.problems, Vec::<String>::new(), "{second:?}");
    let Outcome::Finished {
        session_id: resumed,
        answer,
    } = second.outcome
    else {
        panic!("the resume did not finish: {second:?}");
    };
    assert_eq!(resumed, session_id);
    assert!(
        answer.is_some_and(|answer| answer.to_lowercase().contains("yes")),
        "{resumed}"
    );
}

/// A turn on `runtime` in the work directory of `live`, with `args`.
fn turn_on(live: &Live, runtime: Runtime, program: &str, args: &[&str]) -> Turn {
    Turn {
        runtime,
        program: PathBuf::from(program),
        cwd: live.work(),
        session: Session::New { id: None },
        model: None,
        args: args.iter().map(|arg| arg.to_string()).collect(),
        time_limit: Duration::from_secs(300),
        message: "Reply with the word ok. Do nothing else.".to_string(),
    }
}

/// A random UUID v4, for a session id the caller chooses.
fn new_uuid() -> String {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .unwrap();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[test]
#[ignore = "runs the real claude CLI and uses the account's usage"]
fn the_claude_adapter_holds() {
    validate(RuntimeName::Claude, || {
        // claude 2.1.285 does not apply `--permission-mode auto` on haiku:
        // its init event says `default`, the write asks for a prompt, and
        // nobody answers it. sonnet applies `auto`. A run in another mode
        // fails the test through the problem it reports.
        start_then_continue("claude", Some("sonnet"));

        let live = Live::new("turn-claude");
        let mut turn = turn_on(
            &live,
            Runtime::Claude,
            "claude",
            &["--permission-mode", "dontAsk"],
        );
        turn.model = Some("sonnet".to_string());
        turn.session = Session::New {
            id: Some(new_uuid()),
        };
        turn_then_resume(turn);
    });
}

#[test]
#[ignore = "runs the real codex CLI and uses the account's usage"]
fn the_codex_adapter_holds() {
    validate(RuntimeName::Codex, || {
        start_then_continue("codex", None);

        let live = Live::new("turn-codex");
        let turn = turn_on(
            &live,
            Runtime::Codex,
            "codex",
            &["-c", r#"sandbox_mode="read-only""#],
        );
        turn_then_resume(turn);
    });
}
