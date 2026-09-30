//! The library's turn, with a stub in place of the agent CLI.
//!
//! Each test writes a shell script as the agent. The script logs its
//! arguments, then does what the test says. No test runs a real agent.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use heinzel::{Outcome, Report, Runtime, Session, Turn};

mod versions;

use versions::{claude_version, codex_version, other_claude_version};

const INIT: &str =
    r#"{"type":"system","subtype":"init","session_id":"s-1","permissionMode":"dontAsk"}"#;
const ANSWER: &str =
    r#"{"type":"result","subtype":"success","is_error":false,"result":"the answer"}"#;

/// A scratch directory with a stub agent in it.
struct Stub {
    root: PathBuf,
}

impl Stub {
    /// A stub whose body, after it logs its arguments, is `body` as `sh`.
    /// `--version` prints `version` and nothing else.
    fn new(tag: &str, version: &str, body: &str) -> Self {
        let root = std::env::temp_dir().join(format!("heinzel-turn-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("work")).unwrap();
        let stub = Self { root };
        fs::write(
            stub.program(),
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\n\
                 printf '%s\\n' \"$*\" >> '{calls}'\n\
                 {body}\n",
                calls = stub.calls_log().display(),
            ),
        )
        .unwrap();
        fs::set_permissions(stub.program(), fs::Permissions::from_mode(0o755)).unwrap();
        stub
    }

    fn program(&self) -> PathBuf {
        self.root.join("agent")
    }

    fn calls_log(&self) -> PathBuf {
        self.root.join("calls.log")
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// The arguments of each call of the stub, one entry per call.
    fn calls(&self) -> Vec<String> {
        match fs::read_to_string(self.calls_log()) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// A claude turn in a new session on this stub, with a limit of ten
    /// seconds.
    fn claude(&self) -> Turn {
        Turn {
            runtime: Runtime::Claude,
            program: self.program(),
            cwd: self.work(),
            session: Session::New { id: None },
            model: None,
            args: vec!["--permission-mode".to_string(), "dontAsk".to_string()],
            time_limit: Duration::from_secs(10),
            message: "go".to_string(),
        }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A stub body that prints `lines`, one `echo` each.
fn prints(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| format!("echo '{line}'"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn run(turn: &Turn) -> Report {
    heinzel::run(turn).unwrap()
}

fn failure(report: &Report) -> &str {
    match &report.outcome {
        Outcome::Failed { reason } => reason,
        outcome => panic!("the turn did not fail: {outcome:?}"),
    }
}

/// Whether the process `pid` still exists five seconds from now. A killed
/// process whose parent is gone stays a zombie until init reaps it, so the
/// check gives it that time. A process nobody killed lives on: each test
/// starts it for 300 seconds.
fn alive(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: `kill` with signal 0 only checks, and touches no memory.
    while unsafe { libc::kill(pid, 0) } == 0 {
        if Instant::now() >= deadline {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Wait until `path` holds a process id, and return it.
fn pid_in(path: &PathBuf) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// A finished turn names its session and brings the agent's answer. The
/// caller's arguments reach the command line as given, and nothing else
/// about permissions does.
#[test]
fn a_claude_turn_finishes_with_its_session_and_answer() {
    let stub = Stub::new("claude", &claude_version(), &prints(&[INIT, ANSWER]));
    let mut turn = stub.claude();
    turn.model = Some("sonnet".to_string());
    turn.message = "-go".to_string();
    let report = run(&turn);
    assert_eq!(
        report,
        Report {
            outcome: Outcome::Finished {
                session_id: "s-1".to_string(),
                answer: Some("the answer".to_string()),
            },
            problems: Vec::new(),
        }
    );
    assert_eq!(
        stub.calls(),
        [
            "-p --output-format stream-json --verbose --model sonnet --permission-mode dontAsk -- -go"
        ]
    );
}

/// claude opens a new session under the id the caller chose, and a resume
/// names the session it continues.
#[test]
fn a_claude_turn_takes_the_session_the_caller_names() {
    let stub = Stub::new("claude-ids", &claude_version(), &prints(&[INIT, ANSWER]));
    let mut turn = stub.claude();
    turn.session = Session::New {
        id: Some("s-1".to_string()),
    };
    run(&turn);
    turn.session = Session::Resume {
        id: "s-1".to_string(),
    };
    run(&turn);
    let calls = stub.calls();
    assert!(
        calls[0].contains("--verbose --session-id s-1 --permission-mode"),
        "{calls:?}"
    );
    assert!(
        calls[1].contains("--verbose --resume s-1 --permission-mode"),
        "{calls:?}"
    );
}

/// A session other than the one the turn asked for is no silent success.
#[test]
fn another_session_than_the_one_asked_for_is_a_problem() {
    let stub = Stub::new("claude-other", &claude_version(), &prints(&[INIT, ANSWER]));
    let mut turn = stub.claude();
    turn.session = Session::Resume {
        id: "s-0".to_string(),
    };
    let report = run(&turn);
    assert_eq!(
        report.problems,
        ["the turn asked for the session \"s-0\", and the agent named \"s-1\""]
    );
}

#[test]
fn a_codex_turn_finishes_with_the_last_agent_message() {
    let stub = Stub::new(
        "codex",
        &codex_version(),
        &prints(&[
            r#"{"type":"thread.started","thread_id":"t-1"}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"the answer"}}"#,
            r#"{"type":"turn.completed","usage":{}}"#,
        ]),
    );
    let mut turn = stub.claude();
    turn.runtime = Runtime::Codex;
    turn.args = vec!["-c".to_string(), r#"sandbox_mode="read-only""#.to_string()];
    let report = run(&turn);
    assert_eq!(
        report.outcome,
        Outcome::Finished {
            session_id: "t-1".to_string(),
            answer: Some("the answer".to_string()),
        }
    );
    assert_eq!(
        stub.calls(),
        [r#"exec --json -c sandbox_mode="read-only" -- go"#]
    );
}

/// codex chooses its own session ids. A turn that names one is refused
/// before anything runs.
#[test]
fn a_codex_turn_with_a_chosen_session_id_is_refused() {
    let stub = Stub::new("codex-id", &codex_version(), "");
    let mut turn = stub.claude();
    turn.runtime = Runtime::Codex;
    turn.session = Session::New {
        id: Some("t-1".to_string()),
    };
    let error = heinzel::run(&turn).unwrap_err();
    assert!(error.contains("codex chooses the id"), "{error}");
    assert!(stub.calls().is_empty());
}

#[test]
fn a_turn_without_a_time_limit_is_refused() {
    let stub = Stub::new("no-limit", &claude_version(), "");
    let mut turn = stub.claude();
    turn.time_limit = Duration::ZERO;
    let error = heinzel::run(&turn).unwrap_err();
    assert!(error.contains("time limit"), "{error}");
    assert!(stub.calls().is_empty());
}

/// At the limit the turn stops. The stub does not ignore `SIGTERM`, so the
/// `SIGTERM` ends it, and the turn does not wait for the `SIGKILL`. The
/// `SIGTERM` may come before the shell ran a line, or during the `sleep`:
/// either way it ends the stub.
///
/// That the processes the agent started end too, and that an agent which
/// ignores `SIGTERM` gets `SIGKILL`, is in `tests/caller.rs`. A stub that
/// ignores `SIGTERM` must do so from its start, which needs a caller of its
/// own.
#[test]
fn a_turn_past_its_time_limit_fails() {
    let stub = Stub::new("limit", &claude_version(), "exec sleep 300");
    let mut turn = stub.claude();
    turn.time_limit = Duration::from_millis(500);
    let started = Instant::now();
    let report = run(&turn);
    let elapsed = started.elapsed();
    assert!(elapsed < heinzel::TERM_GRACE, "{elapsed:?}");
    assert!(
        failure(&report).contains("ran past its time limit of 500ms"),
        "{report:?}"
    );
}

/// A process the agent left behind ends with the turn. It holds the
/// agent's stdout open, and the turn still ends when the agent exits.
#[test]
fn a_process_the_agent_left_behind_ends_with_the_turn() {
    let stub = Stub::new("leftover", &claude_version(), "");
    let child_pid = stub.root.join("child.pid");
    fs::write(
        stub.program(),
        format!(
            "#!/bin/sh\nsleep 300 &\necho $! > '{}'\n{}\n",
            child_pid.display(),
            prints(&[INIT, ANSWER])
        ),
    )
    .unwrap();
    let mut turn = stub.claude();
    turn.time_limit = Duration::from_secs(60);
    let started = Instant::now();
    let report = run(&turn);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(
        matches!(report.outcome, Outcome::Finished { .. }),
        "{report:?}"
    );
    let child = pid_in(&child_pid);
    assert!(!alive(child), "the agent's child {child} still runs");
}

#[test]
fn a_claude_usage_limit_is_a_limit_with_its_reset() {
    let rejected = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790000000}}"#;
    let error = r#"{"type":"result","subtype":"success","is_error":true,"result":"limit"}"#;
    let stub = Stub::new(
        "usage",
        &claude_version(),
        &format!("{}\nexit 1", prints(&[INIT, rejected, error])),
    );
    let report = run(&stub.claude());
    assert_eq!(
        report.outcome,
        Outcome::Limited {
            resets_at: Some(1_790_000_000)
        }
    );
}

/// A failure says why: what the runtime said, and the end of the agent's
/// stderr.
#[test]
fn a_failed_turn_says_why() {
    let stub = Stub::new(
        "failed",
        &claude_version(),
        &format!("{}\necho 'Invalid API key' >&2\nexit 3", prints(&[INIT])),
    );
    let reason = failure(&run(&stub.claude())).to_string();
    assert!(reason.contains("exit status: 3"), "{reason}");
    assert!(
        reason.ends_with("its stderr ends: Invalid API key"),
        "{reason}"
    );
}

/// A line the adapter cannot read is a problem, and the turn goes on.
#[test]
fn an_unreadable_line_is_a_problem() {
    let stub = Stub::new(
        "unreadable",
        &claude_version(),
        &prints(&[INIT, "Loading...", ANSWER]),
    );
    let report = run(&stub.claude());
    assert!(
        matches!(report.outcome, Outcome::Finished { .. }),
        "{report:?}"
    );
    assert_eq!(report.problems.len(), 1, "{report:?}");
    assert!(report.problems[0].contains("Loading..."), "{report:?}");
}

#[test]
fn a_program_that_cannot_start_is_an_error() {
    let stub = Stub::new("missing", &claude_version(), "");
    let mut turn = stub.claude();
    turn.program = stub.root.join("no-such-agent");
    let error = heinzel::run(&turn).unwrap_err();
    assert!(error.contains("cannot start"), "{error}");
}

#[test]
fn another_cli_version_gets_a_note() {
    let (other, output) = other_claude_version();
    let stub = Stub::new("version", &output, "");
    let note = heinzel::version_note(Runtime::Claude, &stub.program()).unwrap();
    assert!(note.contains(&format!("{other:?}")), "{note}");
    let stub = Stub::new("same-version", &claude_version(), "");
    assert_eq!(
        heinzel::version_note(Runtime::Claude, &stub.program()),
        None
    );
}
