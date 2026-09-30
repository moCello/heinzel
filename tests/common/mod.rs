//! A scratch bench for the integration tests: a home, a working directory,
//! and a stub in place of the agent CLI.
//!
//! The stub is a shell script each test writes. It logs every call's
//! arguments and environment, then does what the test says. No test runs a
//! real agent.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

pub struct Bench {
    pub root: PathBuf,
    pub home: PathBuf,
    /// The directory the agent runs in.
    pub work: PathBuf,
    pub done_file: PathBuf,
    /// Where codex keeps its session files in these tests.
    pub codex_home: PathBuf,
    /// One line per call of the stub: its arguments.
    calls: PathBuf,
    /// The environment of the last call of the stub.
    env_log: PathBuf,
}

impl Bench {
    pub fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("heinzel-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let bench = Self {
            home: root.join("home"),
            work: root.join("work"),
            done_file: root.join("work/DONE"),
            codex_home: root.join("codex"),
            calls: root.join("calls.log"),
            env_log: root.join("env.log"),
            root,
        };
        for dir in [&bench.home, &bench.work, &bench.codex_home] {
            fs::create_dir_all(dir).unwrap();
        }
        bench
    }

    /// Put a stub in place of both CLIs. After it logs the call, it runs
    /// `body` as `sh`. `--version` prints `version` and nothing else.
    pub fn agent(&self, version: &str, body: &str) {
        let script = self.root.join("agent");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\n\
                 printf '%s\\n' \"$*\" >> '{calls}'\n\
                 env > '{env}'\n\
                 {body}\n",
                calls = self.calls.display(),
                env = self.env_log.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            self.home.join("config.toml"),
            format!(
                "[claude]\nprogram = '{0}'\n[codex]\nprogram = '{0}'\n",
                script.display()
            ),
        )
        .unwrap();
    }

    /// heinzel, ready to run with `args` on this bench.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_heinzel"));
        command
            .args(args)
            .current_dir(&self.work)
            .env("HEINZEL_HOME", &self.home)
            .env("CODEX_HOME", &self.codex_home)
            // As if heinzel ran inside a claude session.
            .env("CLAUDECODE", "1")
            .env("CLAUDE_CODE_SESSION_ID", "the-parent-session");
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Run heinzel with `args`, expect success, and return the JSON it
    /// printed last. An open prints what the agent printed before it.
    pub fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "heinzel {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        serde_json::from_str(stdout.lines().last().unwrap_or_default()).unwrap()
    }

    /// Run heinzel with `args`, expect failure, and return what it printed
    /// to stderr.
    pub fn refused(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            !output.status.success(),
            "heinzel {args:?} succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8(output.stderr).unwrap()
    }

    /// Start `key` on `runtime` with the message `go`.
    pub fn start(&self, key: &str, runtime: &str) -> Value {
        let done = self.done_file.display().to_string();
        self.ok(&[
            "start",
            key,
            "--runtime",
            runtime,
            "--done-file",
            &done,
            "--message",
            "go",
        ])
    }

    /// The arguments of each call of the stub, one entry per call.
    pub fn calls(&self) -> Vec<String> {
        match fs::read_to_string(&self.calls) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The environment of the last call of the stub.
    pub fn agent_env(&self) -> String {
        fs::read_to_string(&self.env_log).unwrap()
    }

    /// Wait until `path` exists.
    pub fn await_file(&self, path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The lines of a claude stream: the init event of session `s-1` in the
/// permission mode `auto`, which the default profile asks for, then
/// `lines`.
pub fn claude_stream(lines: &[&str]) -> String {
    claude_stream_in("auto", lines)
}

/// [`claude_stream`], with claude applying the permission mode `mode`.
pub fn claude_stream_in(mode: &str, lines: &[&str]) -> String {
    let mut body = format!(
        r#"echo '{{"type":"system","subtype":"init","session_id":"s-1","permissionMode":"{mode}"}}'"#
    );
    for line in lines {
        body.push_str(&format!("\necho '{line}'"));
    }
    body
}

pub const CLAUDE_SUCCESS: &str = r#"{"type":"result","subtype":"success","is_error":false}"#;
pub const CLAUDE_VERSION: &str = "2.1.285 (Claude Code)";
pub const CODEX_VERSION: &str = "codex-cli 0.159.0";
