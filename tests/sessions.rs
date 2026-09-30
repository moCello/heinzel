//! Sessions end to end: the heinzel binary, with a stub in place of the
//! agent CLI.

use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod common;
mod versions;

use common::{Bench, CLAUDE_SUCCESS, claude_stream, claude_stream_in};
use heinzel_runtime::RuntimeName;
use versions::{claude_version, codex_version, other_claude_version};

/// A stub body that keeps the agent alive until a stop.
const LINGER: &str = "sleep 30";

fn state(report: &Value) -> &str {
    report["state"].as_str().unwrap()
}

/// Send `signal` to the process `pid`, or to the group it leads when
/// `group` holds.
fn signal(pid: &Value, signal: i32, group: bool) {
    let pid = i32::try_from(pid.as_u64().unwrap()).unwrap();
    let target = if group { -pid } else { pid };
    // SAFETY: `kill` takes plain integers and touches no memory.
    unsafe { libc::kill(target, signal) };
}

#[test]
fn a_run_that_writes_the_done_file_is_done() {
    let bench = Bench::new("done");
    let done = bench.done_file.display().to_string();
    bench.agent(
        &claude_version(),
        &format!("{}\ntouch '{done}'", claude_stream(&[CLAUDE_SUCCESS])),
    );
    let started = bench.start("job", "claude");
    assert_eq!(state(&started), "running");
    let ended = bench.ok(&["wait", "job"]);
    assert_eq!(state(&ended), "done");
    assert_eq!(ended["session_id"], "s-1");
    assert_eq!(ended["run"], 1);
    assert_eq!(
        bench.calls(),
        [
            "-p --output-format stream-json --verbose --name job --permission-mode auto \
--permission-prompts none -- go"
        ]
    );
}

/// A run that finished its turn without the done file stopped to ask.
#[test]
fn a_run_that_finishes_without_the_done_file_is_a_question() {
    let bench = Bench::new("question");
    bench.agent(&claude_version(), &claude_stream(&[CLAUDE_SUCCESS]));
    bench.start("job", "claude");
    assert_eq!(state(&bench.ok(&["wait", "job"])), "question");
}

#[test]
fn a_run_that_exits_nonzero_failed() {
    let bench = Bench::new("failed");
    bench.agent(
        &claude_version(),
        &format!("{}\nexit 3", claude_stream(&[CLAUDE_SUCCESS])),
    );
    bench.start("job", "claude");
    let ended = bench.ok(&["wait", "job"]);
    assert_eq!(state(&ended), "failed");
    assert!(
        ended["reason"].as_str().unwrap().contains("exit status: 3"),
        "{ended}"
    );
}

#[test]
fn a_rejected_claude_rate_limit_is_a_limit() {
    let bench = Bench::new("claude-limit");
    bench.agent(
        &claude_version(),
        &format!(
            "{}\nexit 1",
            claude_stream(&[
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790000000}}"#,
                r#"{"type":"result","subtype":"success","is_error":true,"result":"limit"}"#,
            ])
        ),
    );
    bench.start("job", "claude");
    let ended = bench.ok(&["wait", "job"]);
    assert_eq!(state(&ended), "limited");
    assert_eq!(ended["resets_at"], 1_790_000_000);
}

const CODEX_ID: &str = "019a0c2e-7a3b-7c11-9d0e-5f6a7b8c9d0e";

/// A codex stub that opens `CODEX_ID`, records `used` percent of its
/// primary window in the session file, and fails the turn.
fn codex_failing_at(bench: &Bench, used: f64) {
    let day = bench.codex_home.join("sessions/2026/09/30");
    let file = day.join(format!("rollout-2026-09-30T12-00-00-{CODEX_ID}.jsonl"));
    let record = format!(
        r#"{{"timestamp":"2026-09-30T12:00:01Z","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"primary":{{"used_percent":{used},"window_minutes":300,"resets_at":1790000000}},"secondary":null}}}}}}"#
    );
    bench.agent(
        &codex_version(),
        &format!(
            "mkdir -p '{day}'\n\
             echo '{record}' >> '{file}'\n\
             echo '{{\"type\":\"thread.started\",\"thread_id\":\"{CODEX_ID}\"}}'\n\
             echo '{{\"type\":\"turn.started\"}}'\n\
             echo '{{\"type\":\"turn.failed\",\"error\":{{\"message\":\"usage limit\"}}}}'\n\
             exit 1",
            day = day.display(),
            file = file.display(),
        ),
    );
}

/// codex says no more than a message in its stream. The session file shows
/// the full window and when it resets.
#[test]
fn a_full_codex_window_is_a_limit() {
    let bench = Bench::new("codex-limit");
    codex_failing_at(&bench, 100.0);
    bench.start("job", "codex");
    let ended = bench.ok(&["wait", "job"]);
    assert_eq!(state(&ended), "limited", "{ended}");
    assert_eq!(ended["resets_at"], 1_790_000_000);
    assert_eq!(ended["session_id"], CODEX_ID);
}

#[test]
fn a_codex_failure_below_the_limit_failed() {
    let bench = Bench::new("codex-failed");
    codex_failing_at(&bench, 50.0);
    bench.start("job", "codex");
    let ended = bench.ok(&["wait", "job"]);
    assert_eq!(state(&ended), "failed", "{ended}");
    assert_eq!(ended["reason"], "codex: usage limit");
}

/// A `result` is not a stop: the agent lives on until a stop ends it.
#[test]
fn a_stop_ends_a_run_that_outlives_its_result() {
    let bench = Bench::new("stop");
    bench.agent(
        &claude_version(),
        &format!("{}\n{LINGER}", claude_stream(&[CLAUDE_SUCCESS])),
    );
    bench.start("job", "claude");
    let stream = bench.home.join("sessions/job/stream.jsonl");
    bench.await_file(&stream);
    while !fs::read_to_string(&stream).unwrap().contains("result") {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(state(&bench.ok(&["status", "job"])), "running");
    let stopped = bench.ok(&["stop", "job"]);
    assert_eq!(state(&stopped), "stopped");
    assert_eq!(state(&bench.ok(&["status", "job"])), "stopped");
}

/// A running session refuses a continue, and the agent is not called again.
#[test]
fn a_continue_is_refused_while_a_run_holds_the_session() {
    let bench = Bench::new("continue-busy");
    bench.agent(
        &claude_version(),
        &format!("{}\n{LINGER}", claude_stream(&[])),
    );
    bench.start("job", "claude");
    let refusal = bench.refused(&["continue", "job", "--message", "again"]);
    assert!(refusal.contains("has a writer already"), "{refusal}");
    assert_eq!(bench.calls().len(), 1, "{:?}", bench.calls());
    bench.ok(&["stop", "job"]);
}

/// A running session refuses an open, and the agent is not called again.
#[test]
fn an_open_is_refused_while_a_run_holds_the_session() {
    let bench = Bench::new("open-busy");
    bench.agent(
        &claude_version(),
        &format!("{}\n{LINGER}", claude_stream(&[])),
    );
    bench.start("job", "claude");
    bench.await_file(&bench.home.join("sessions/job/stream.jsonl"));
    let refusal = bench.refused(&["open", "job"]);
    assert!(refusal.contains("has a writer already"), "{refusal}");
    assert_eq!(bench.calls().len(), 1, "{:?}", bench.calls());
    bench.ok(&["stop", "job"]);
}

/// A stopped session stays reachable: a continue resumes it headless, and
/// an open resumes it in the terminal. Neither runs a permission profile
/// the session did not name, and the open runs none.
#[test]
fn a_stopped_session_can_be_continued_and_opened() {
    let bench = Bench::new("reachable");
    bench.agent(&claude_version(), &claude_stream(&[CLAUDE_SUCCESS]));
    bench.start("job", "claude");
    bench.ok(&["wait", "job"]);
    let resumed = bench.ok(&["continue", "job", "--message", "and now?"]);
    assert_eq!(resumed["run"], 2);
    let ended = bench.ok(&["wait", "job"]);
    assert_eq!((state(&ended), &ended["run"]), ("question", &json!(2)));
    let opened = bench.ok(&["open", "job"]);
    assert_eq!((state(&opened), &opened["run"]), ("closed", &json!(3)));
    let calls = bench.calls();
    assert_eq!(
        calls[1],
        "-p --output-format stream-json --verbose --resume s-1 --permission-mode auto \
--permission-prompts none -- and now?"
    );
    assert_eq!(calls[2], "--resume s-1");
}

/// codex chooses its own session id. heinzel records it from the stream
/// and a continue names it.
#[test]
fn a_codex_continue_names_the_session_codex_chose() {
    let bench = Bench::new("codex-resume");
    bench.agent(
        &codex_version(),
        &format!(
            "echo '{{\"type\":\"thread.started\",\"thread_id\":\"{CODEX_ID}\"}}'\n\
             echo '{{\"type\":\"turn.completed\",\"usage\":{{}}}}'"
        ),
    );
    bench.start("job", "codex");
    bench.ok(&["wait", "job"]);
    bench.ok(&["continue", "job", "--message", "more"]);
    assert_eq!(state(&bench.ok(&["wait", "job"])), "question");
    let calls = bench.calls();
    assert!(calls[0].starts_with("exec --json -c "), "{}", calls[0]);
    assert!(
        calls[1].starts_with("exec resume --json -c ")
            && calls[1].ends_with(&format!("-- {CODEX_ID} more")),
        "{}",
        calls[1]
    );
}

#[test]
fn the_no_checks_profile_runs_only_when_a_start_names_it() {
    let bench = Bench::new("no-checks");
    bench.agent(
        &claude_version(),
        &claude_stream_in("bypassPermissions", &[CLAUDE_SUCCESS]),
    );
    let done = bench.done_file.display().to_string();
    bench.ok(&[
        "start",
        "job",
        "--runtime",
        "claude",
        "--done-file",
        &done,
        "--profile",
        "no-checks",
        "--message",
        "go",
    ]);
    let ended = bench.ok(&["wait", "job"]);
    assert_eq!(ended.get("problems"), None, "{ended}");
    assert!(
        bench.calls()[0].contains("--dangerously-skip-permissions"),
        "{:?}",
        bench.calls()
    );
}

/// claude 2.1.285 runs `auto` as `default` on some models. The run goes
/// on, and the caller sees that the mode is not the one the profile asked
/// for.
#[test]
fn a_permission_mode_claude_overrode_is_a_problem_the_caller_sees() {
    let bench = Bench::new("overridden");
    bench.agent(
        &claude_version(),
        &claude_stream_in("default", &[CLAUDE_SUCCESS]),
    );
    bench.start("job", "claude");
    let ended = bench.ok(&["wait", "job"]);
    assert_eq!(state(&ended), "question");
    assert_eq!(
        ended["problems"],
        json!([
            "the profile asked claude for the permission mode \"auto\", and claude applied \
\"default\""
        ])
    );
}

/// The agent does not inherit the identity of the agent session heinzel
/// runs in.
#[test]
fn the_agent_does_not_inherit_the_parent_session() {
    let bench = Bench::new("env");
    bench.agent(&claude_version(), &claude_stream(&[CLAUDE_SUCCESS]));
    bench.start("job", "claude");
    bench.ok(&["wait", "job"]);
    let env = bench.agent_env();
    assert!(env.contains("HEINZEL_HOME="), "the env log is empty");
    assert!(!env.contains("CLAUDECODE="), "{env}");
    assert!(!env.contains("CLAUDE_CODE_SESSION_ID="), "{env}");
}

/// The holder survives the process group of whatever started it.
#[test]
fn the_holder_outlives_its_callers_process_group() {
    let bench = Bench::new("detached");
    let done = bench.done_file.display().to_string();
    bench.agent(
        &claude_version(),
        &format!(
            "sleep 1\n{}\ntouch '{done}'",
            claude_stream(&[CLAUDE_SUCCESS])
        ),
    );
    let mut start = bench.command(&[
        "start",
        "job",
        "--runtime",
        "claude",
        "--done-file",
        &done,
        "--message",
        "go",
    ]);
    start.process_group(0);
    let child = start.spawn().unwrap();
    let group = json!(child.id());
    assert!(child.wait_with_output().unwrap().status.success());
    // An empty group is the outcome the test wants: nothing left to kill.
    signal(&group, libc::SIGKILL, true);
    assert_eq!(state(&bench.ok(&["wait", "job"])), "done");
}

/// A holder that dies without a word leaves a live state and a free lock.
/// A reader sees an error, not a running session.
#[test]
fn a_lost_holder_is_an_error() {
    let bench = Bench::new("lost");
    bench.agent(
        &claude_version(),
        &format!("{}\n{LINGER}", claude_stream(&[])),
    );
    let started = bench.start("job", "claude");
    signal(&started["holder_pid"], libc::SIGKILL, false);
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = loop {
        let output = bench.run(&["status", "job"]);
        if !output.status.success() {
            break String::from_utf8(output.stderr).unwrap();
        }
        assert!(Instant::now() < deadline, "status never failed");
    };
    assert!(error.contains("ended without recording a stop"), "{error}");

    // The agent of the lost holder may still run, so no second writer
    // starts: not headless, not in a terminal.
    for refused in [
        &["continue", "job", "--message", "again"][..],
        &["open", "job"],
    ] {
        let refusal = bench.refused(refused);
        assert!(
            refusal.contains("ended without recording a stop"),
            "{refused:?}: {refusal}"
        );
    }
    assert_eq!(bench.calls().len(), 1, "{:?}", bench.calls());

    // A stop ends the agent that the state names, and the key is usable
    // again.
    let stopped = bench.ok(&["stop", "job"]);
    assert_eq!(state(&stopped), "stopped");
    let agent = i32::try_from(started["agent_pid"].as_u64().unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    // SAFETY: `kill` takes plain integers and touches no memory. Signal 0
    // only asks whether the group exists.
    while unsafe { libc::kill(-agent, 0) } == 0 {
        assert!(Instant::now() < deadline, "the agent outlived the stop");
        thread::sleep(Duration::from_millis(20));
    }
    bench.ok(&["continue", "job", "--message", "again"]);
    bench.ok(&["stop", "job"]);
}

/// After a lost holder whose agent is gone too, the recorded id may name
/// another process group by now. A stop records the stop and signals no
/// one.
#[test]
fn a_stop_after_a_lost_holder_spares_a_reused_group_id() {
    let bench = Bench::new("reused");
    bench.agent(
        &claude_version(),
        &format!("{}\n{LINGER}", claude_stream(&[])),
    );
    let started = bench.start("job", "claude");
    signal(&started["holder_pid"], libc::SIGKILL, false);
    signal(&started["agent_pid"], libc::SIGKILL, true);
    let deadline = Instant::now() + Duration::from_secs(10);
    while bench.run(&["status", "job"]).status.success() {
        assert!(Instant::now() < deadline, "status never failed");
        thread::sleep(Duration::from_millis(20));
    }

    // A process group that took the agent's id after the agent was gone.
    let mut stranger = Command::new("sleep");
    stranger.arg("30").process_group(0);
    let mut stranger = stranger.spawn().unwrap();
    let state_file = bench.home.join("sessions/job/state.json");
    let mut recorded: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    recorded["agent_pid"] = json!(stranger.id());
    fs::write(&state_file, recorded.to_string()).unwrap();

    // The agent's processes release the agent lock as they die.
    let agent_lock = File::open(bench.home.join("sessions/job/agent.lock")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while agent_lock.try_lock_shared().is_err() {
        assert!(
            Instant::now() < deadline,
            "the agent never released its lock"
        );
        thread::sleep(Duration::from_millis(20));
    }
    agent_lock.unlock().unwrap();

    let stopped = bench.ok(&["stop", "job"]);
    assert_eq!(state(&stopped), "stopped");
    thread::sleep(Duration::from_millis(100));
    let spared = stranger.try_wait().unwrap().is_none();
    let _ = stranger.kill();
    let _ = stranger.wait();
    assert!(spared, "the stop killed a group that is not the agent's");
}

/// A first run that never reported a session id leaves nothing to resume.
/// A continue then runs a fresh turn under the same key.
#[test]
fn a_continue_without_a_session_id_starts_fresh() {
    let bench = Bench::new("no-session");
    bench.agent(&claude_version(), "exit 1");
    bench.start("job", "claude");
    let failed = bench.ok(&["wait", "job"]);
    assert_eq!(
        (state(&failed), &failed["session_id"]),
        ("failed", &Value::Null)
    );
    bench.agent(&claude_version(), &claude_stream(&[CLAUDE_SUCCESS]));
    bench.ok(&["continue", "job", "--message", "again"]);
    let ended = bench.ok(&["wait", "job"]);
    assert_eq!(
        (state(&ended), &ended["session_id"]),
        ("question", &json!("s-1"))
    );
    let calls = bench.calls();
    assert!(
        calls[1].contains("--name job") && !calls[1].contains("--resume"),
        "{calls:?}"
    );
}

/// An open whose agent writes the done file ends done.
#[test]
fn an_open_that_writes_the_done_file_is_done() {
    let bench = Bench::new("open-done");
    let done = bench.done_file.display().to_string();
    bench.agent(
        &claude_version(),
        &format!(
            "if [ \"$1\" = --resume ]; then touch '{done}'; exit 0; fi\n{}",
            claude_stream(&[CLAUDE_SUCCESS])
        ),
    );
    bench.start("job", "claude");
    assert_eq!(state(&bench.ok(&["wait", "job"])), "question");
    let opened = bench.ok(&["open", "job"]);
    assert_eq!((state(&opened), &opened["run"]), ("done", &json!(2)));
}

#[test]
fn a_start_refuses_a_key_that_has_a_session() {
    let bench = Bench::new("taken");
    bench.agent(&claude_version(), &claude_stream(&[CLAUDE_SUCCESS]));
    bench.start("job", "claude");
    let done = bench.done_file.display().to_string();
    let refusal = bench.refused(&[
        "start",
        "job",
        "--runtime",
        "claude",
        "--done-file",
        &done,
        "--message",
        "go",
    ]);
    assert!(refusal.contains("exists already"), "{refusal}");
}

/// A line the adapter cannot read does not stop the run, and the caller
/// sees it.
#[test]
fn a_bad_stream_line_is_a_problem_the_caller_sees() {
    let bench = Bench::new("bad-line");
    bench.agent(
        &claude_version(),
        &format!("echo 'not json'\n{}", claude_stream(&[CLAUDE_SUCCESS])),
    );
    bench.start("job", "claude");
    let ended = bench.ok(&["wait", "job"]);
    assert_eq!(state(&ended), "question");
    let problems = ended["problems"].as_array().unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].as_str().unwrap().contains("not json"));
}

/// A CLI other than the validated version runs, with a warning.
#[test]
fn another_cli_version_warns() {
    let bench = Bench::new("version");
    let (other, output) = other_claude_version();
    bench.agent(&output, &claude_stream(&[CLAUDE_SUCCESS]));
    let done = bench.done_file.display().to_string();
    let output = bench.run(&[
        "start",
        "job",
        "--runtime",
        "claude",
        "--done-file",
        &done,
        "--message",
        "go",
    ]);
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let validated = RuntimeName::Claude.adapter().validated_version();
    assert!(
        stderr.contains(&format!("{other:?}")) && stderr.contains(validated),
        "{stderr}"
    );
}
