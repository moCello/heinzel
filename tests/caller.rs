//! Turns whose caller is a process of its own.
//!
//! This test binary plays both parts. Run as
//! `caller caller <program> <work> <limit-ms> <term>`, it is a caller that
//! runs one turn on the stub `program` and waits for it. `<term>` is
//! `ignore` or `default`: what the caller does with `SIGTERM` while it runs
//! the turn. Otherwise it runs the tests.
//!
//! A caller of its own serves two kinds of test:
//!
//! - A test that kills the caller: a turn stops when its caller dies.
//! - A test whose stub ignores `SIGTERM` from its first instruction. A
//!   `trap` in the stub comes too late: the `SIGTERM` can arrive before the
//!   shell runs its first line. An ignored signal stays ignored across
//!   `exec`, and `std` resets only `SIGPIPE`. So a caller that ignores
//!   `SIGTERM` starts a stub that ignores it too. The test process cannot
//!   be that caller: its tests run in parallel, and it would itself ignore
//!   `SIGTERM`.

use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use heinzel::{Runtime, Session, Turn};

/// The first argument that makes this binary a caller.
const CALLER: &str = "caller";
/// The `<term>` argument of a caller that ignores `SIGTERM`.
const IGNORE: &str = "ignore";
/// The `<term>` argument of a caller that takes `SIGTERM`'s default action.
const DEFAULT: &str = "default";

/// How long a test waits past the grace period for a group to end, or for
/// a turn to return.
const MARGIN: Duration = Duration::from_secs(3);

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Some((CALLER, rest)) = args
        .split_first()
        .map(|(first, rest)| (first.as_str(), rest))
    {
        return caller(rest);
    }
    // Any other argument, such as a filter from `cargo test`, is ignored.
    let tests: [(&str, fn()); 3] = [
        (
            "a_turn_ends_when_its_caller_gets_sigkill",
            a_turn_ends_when_its_caller_gets_sigkill,
        ),
        (
            "a_turn_ends_when_its_callers_foreground_group_gets_sigint",
            a_turn_ends_when_its_callers_foreground_group_gets_sigint,
        ),
        (
            "an_agent_that_ignores_sigterm_is_killed_with_its_processes",
            an_agent_that_ignores_sigterm_is_killed_with_its_processes,
        ),
    ];
    let mut failed = 0;
    for (name, test) in tests {
        println!("test {name} ...");
        // The panic hook prints why a test failed. The next test runs all
        // the same.
        if panic::catch_unwind(test).is_ok() {
            println!("test {name} ... ok");
        } else {
            println!("test {name} ... FAILED");
            failed += 1;
        }
    }
    println!("{failed} of {} tests failed", tests.len());
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Run one turn as a caller that waits. It prints how long the turn took,
/// in milliseconds, on one line, and the report on the next.
fn caller(args: &[String]) -> ExitCode {
    let [program, work, limit_ms, term] = args else {
        eprintln!("usage: caller {CALLER} <program> <work> <limit-ms> <{IGNORE}|{DEFAULT}>");
        return ExitCode::FAILURE;
    };
    let Ok(limit_ms) = limit_ms.parse() else {
        eprintln!("{limit_ms:?} is no number of milliseconds");
        return ExitCode::FAILURE;
    };
    let term = match term.as_str() {
        IGNORE => libc::SIG_IGN,
        DEFAULT => libc::SIG_DFL,
        other => {
            eprintln!("{other:?} is neither {IGNORE} nor {DEFAULT}");
            return ExitCode::FAILURE;
        }
    };
    // A caller at a terminal ends on a Ctrl-C. The process that started
    // this one may ignore `SIGINT`, and that would pass on to here.
    // SAFETY: `signal` sets an action that needs no handler, and touches no
    // memory.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, term);
    }
    let turn = Turn {
        runtime: Runtime::Claude,
        program: PathBuf::from(program),
        cwd: PathBuf::from(work),
        session: Session::New { id: None },
        model: None,
        args: Vec::new(),
        time_limit: Duration::from_millis(limit_ms),
        message: "go".to_string(),
    };
    let started = Instant::now();
    let report = heinzel::run(&turn);
    println!("{}", started.elapsed().as_millis());
    println!("{report:?}");
    ExitCode::SUCCESS
}

/// A scratch directory with a stub agent in it. The stub starts a child
/// that runs for 300 seconds, writes its process group id to `group`, and
/// waits.
struct Stub {
    root: PathBuf,
}

impl Stub {
    /// `trap` is the stub's first line.
    fn new(tag: &str, trap: &str) -> Self {
        let root = env::temp_dir().join(format!("heinzel-caller-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("work")).unwrap();
        let stub = Self { root };
        fs::write(
            stub.program(),
            format!(
                "#!/bin/sh\n\
                 {trap}\n\
                 sleep 300 &\n\
                 ps -o pgid= -p $$ > '{group}.tmp'\n\
                 mv '{group}.tmp' '{group}'\n\
                 wait\n",
                group = stub.group_file().display(),
            ),
        )
        .unwrap();
        fs::set_permissions(stub.program(), fs::Permissions::from_mode(0o755)).unwrap();
        stub
    }

    fn program(&self) -> PathBuf {
        self.root.join("agent")
    }

    fn group_file(&self) -> PathBuf {
        self.root.join("group")
    }

    /// Start a caller of a turn on this stub, with the time limit `limit`
    /// and `SIGTERM` as `term` says. It leads a process group of its own,
    /// as a job in a terminal does.
    fn start_caller(&self, limit: Duration, term: &str) -> Child {
        Command::new(env::current_exe().unwrap())
            .arg(CALLER)
            .arg(self.program())
            .arg(self.root.join("work"))
            .arg(limit.as_millis().to_string())
            .arg(term)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Wait until `path` holds a number, and return it.
fn number_in(path: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(number) = text.trim().parse()
        {
            return number;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Whether the process group `group` still exists. macOS answers `EPERM`
/// for a group of processes that exited and wait to be reaped, so only
/// `ESRCH` counts as gone.
fn group_exists(group: i32) -> bool {
    // SAFETY: `kill` with signal 0 only checks, and touches no memory.
    if unsafe { libc::kill(-group, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Wait until the process group `group` is gone, at the latest at
/// `deadline`. `since` is the moment the wait counts from, for the message.
fn group_ends_by(group: i32, since: Instant, deadline: Instant) {
    while group_exists(group) {
        if Instant::now() >= deadline {
            // Leave nothing behind for the next test.
            // SAFETY: `kill` takes plain integers and touches no memory.
            unsafe { libc::kill(-group, libc::SIGKILL) };
            panic!(
                "the turn's group {group} still runs {:?} later",
                since.elapsed()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Start a caller of a turn on a stub, kill the caller with `kill`, and
/// check that the turn's group ends within the grace period.
fn a_turn_ends_when_its_caller_dies(
    tag: &str,
    trap: &str,
    signal: libc::c_int,
    kill: impl FnOnce(&Child),
) {
    let stub = Stub::new(tag, trap);
    let mut caller = stub.start_caller(Duration::from_secs(300), DEFAULT);
    // The stub writes its group after its `trap`, so the trap is in place.
    let group = number_in(&stub.group_file());
    assert!(group_exists(group), "the turn's group {group} never ran");
    let killed = Instant::now();
    kill(&caller);
    let status = caller.wait().unwrap();
    assert_eq!(status.signal(), Some(signal), "{status}");
    group_ends_by(group, killed, killed + heinzel::TERM_GRACE + MARGIN);
    println!(
        "the turn's group ended {:?} after its caller died",
        killed.elapsed()
    );
}

/// A `SIGKILL` gives the caller no chance to clean up. The stub ignores
/// `SIGTERM`, so the guard's `SIGKILL` after the grace period ends it.
fn a_turn_ends_when_its_caller_gets_sigkill() {
    a_turn_ends_when_its_caller_dies("sigkill", "trap '' TERM", libc::SIGKILL, |caller| {
        let pid = i32::try_from(caller.id()).unwrap();
        // SAFETY: `kill` takes plain integers and touches no memory.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    });
}

/// A Ctrl-C at a terminal sends `SIGINT` to the terminal's foreground
/// group. Here that group is the caller's own, as for a job in a shell. The
/// turn's group is not in it.
fn a_turn_ends_when_its_callers_foreground_group_gets_sigint() {
    a_turn_ends_when_its_caller_dies("sigint", "", libc::SIGINT, |caller| {
        let group = i32::try_from(caller.id()).unwrap();
        // SAFETY: `kill` takes plain integers and touches no memory.
        assert_eq!(unsafe { libc::kill(-group, libc::SIGINT) }, 0);
    });
}

/// An agent that ignores `SIGTERM` gets `SIGKILL` after the grace period,
/// and so does the child it started.
///
/// The caller ignores `SIGTERM`, so the stub ignores it from its start,
/// whatever the load. The stub then ends only by the `SIGKILL`, which comes
/// 5 seconds after the limit of 200 milliseconds. In that time it starts
/// its child and writes its group, which takes it milliseconds.
fn an_agent_that_ignores_sigterm_is_killed_with_its_processes() {
    let stub = Stub::new("ignores-term", "");
    let caller = stub.start_caller(Duration::from_millis(200), IGNORE);
    let output = caller.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", output.status);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut lines = stdout.lines();
    let elapsed = Duration::from_millis(lines.next().unwrap().parse().unwrap());
    let report = lines.next().unwrap();
    assert!(elapsed >= heinzel::TERM_GRACE, "{elapsed:?}: {report}");
    assert!(
        elapsed < heinzel::TERM_GRACE + MARGIN,
        "{elapsed:?}: {report}"
    );
    assert!(
        report.contains("ran past its time limit of 200ms"),
        "{report}"
    );
    // The stub wrote its group after it started its child, so the child is
    // in the group, and the group's end is the child's end too.
    let group = number_in(&stub.group_file());
    let returned = Instant::now();
    group_ends_by(group, returned, returned + MARGIN);
}
