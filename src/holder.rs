//! The processes that hold a session's writer lock.
//!
//! A headless run belongs to a detached holder: `heinzel hold`, which
//! [`launch`] starts in a session of its own. The holder outlives its caller,
//! whether that is a shell that closes or a tool call that kills its process
//! group. It takes the writer lock, runs the agent, and records each
//! transition in the state file. When the agent exits, the holder records
//! how the run ended, and the lock goes with the holder.
//!
//! An interactive [`open`] holds the lock in the foreground instead, for as
//! long as the agent runs in the caller's terminal.
//!
//! The lock is the one-writer rule. A second writer finds it held and is
//! refused, whether it is a continue or an open.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{self, Child, Command, Stdio};
use std::time::SystemTime;

use crate::config::Config;
use crate::runtime::{self, Ended, Headless, Turn, Watch};
use crate::state::{Snapshot, State};
use crate::store::{Acquired, HOME_VAR, Home, Key, Record, SessionDir, WriterLock};

/// The internal command that runs a holder.
pub const HOLD_COMMAND: &str = "hold";

/// What a holder reports to the caller that launched it, as the one line it
/// prints. Any other line is an error, which the line states.
const REPORT_STARTED: &str = "started";
const REPORT_BUSY: &str = "busy";

/// The most problems a run records. A stream that is not what the adapter
/// expects may hold a problem on every line.
const MAX_PROBLEMS: usize = 20;

/// How a launch went.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// The holder runs the agent, and the state says `running`.
    Started,
    /// Another writer holds the session.
    Busy,
}

/// The refusal a second writer gets.
pub fn busy(key: &Key) -> String {
    format!(
        "{:?} has a writer already: a headless run or an open holds it, and a session takes one \
writer at a time",
        key.as_str()
    )
}

/// Start a detached holder for a headless run of the session in `dir`, and
/// hand it `message`. Returns once the holder has reported.
///
/// The run continues the session when the record names a session id, and
/// opens one when it does not: a first run that never reported an id left
/// nothing to continue.
pub fn launch(home: &Home, dir: &SessionDir, message: &str) -> Result<Launch, String> {
    let exe = env::current_exe().map_err(|e| format!("cannot find the heinzel program: {e}"))?;
    let log = append_to(&dir.holder_log())?;
    let mut command = Command::new(exe);
    command
        .args([HOLD_COMMAND, dir.key().as_str()])
        .env(HOME_VAR, home.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log);
    // SAFETY: the closure runs in the child between fork and exec, where
    // only async-signal-safe calls are sound. It makes one, `setsid`, and
    // touches no memory it shares with the parent.
    //
    // `setsid` puts the holder in a session and a process group of its own,
    // with no terminal. A signal to the caller's group, or the hangup of the
    // caller's terminal, does not reach it.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start the holder: {e}"))?;
    let written = match child.stdin.take() {
        Some(mut stdin) => stdin.write_all(message.as_bytes()),
        None => Err(io::Error::other("the holder has no stdin")),
    };
    let mut report = String::new();
    if let Some(stdout) = child.stdout.take() {
        BufReader::new(stdout)
            .read_line(&mut report)
            .map_err(|e| format!("cannot read the holder's report: {e}"))?;
    }
    written.map_err(|e| format!("cannot hand the message to the holder: {e}"))?;
    match report.trim_end() {
        REPORT_STARTED => Ok(Launch::Started),
        REPORT_BUSY => Ok(Launch::Busy),
        "" => Err(format!(
            "the holder ended before it reported: see {}",
            dir.holder_log().display()
        )),
        error => Err(error.to_string()),
    }
}

/// Run as the holder of one headless run of `key`. The message comes on
/// stdin, and the report goes to stdout as one line.
pub fn hold(home: &Home, key: &Key) -> Result<(), String> {
    let mut message = String::new();
    io::stdin()
        .read_to_string(&mut message)
        .map_err(|e| format!("cannot read the message: {e}"))?;
    let dir = home.session(key);
    let lock = match dir.acquire() {
        Ok(Acquired::Held(lock)) => lock,
        Ok(Acquired::Busy) => return report(REPORT_BUSY),
        Err(e) => {
            report(&e)?;
            return Err(e);
        }
    };
    match Run::start(home, &lock, &message) {
        Ok(run) => {
            // The agent runs, and the state says so. A caller that is gone
            // before it reads the report changes neither, so the holder
            // follows the run all the same.
            let _ = report(REPORT_STARTED);
            run.follow(&lock)
        }
        Err(e) => {
            report(&e)?;
            Err(e)
        }
    }
}

/// Print the one line of the report.
fn report(line: &str) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{}", line.replace('\n', " "))
        .and_then(|()| stdout.flush())
        .map_err(|e| format!("cannot report to the caller: {e}"))
}

/// A headless run in progress.
struct Run {
    child: Child,
    watch: Box<dyn Watch>,
    record: Record,
    run: u32,
    done_before: DoneMark,
}

impl Run {
    /// Spawn the agent and record the session as running.
    ///
    /// An error before the spawn leaves the state as it was: no run
    /// happened. A spawn that fails is a run that failed, and says so.
    fn start(home: &Home, lock: &WriterLock, message: &str) -> Result<Self, String> {
        let dir = lock.dir();
        let run = lock.next_run()?;
        let record = dir.read_record()?;
        let config = Config::load(home)?;
        let settings = config.runtime(record.runtime);
        let profile = settings.profile(&record.profile)?;
        let turn = match record.session_id.as_deref() {
            Some(session_id) => Turn::Resume { session_id },
            None => Turn::Fresh {
                key: dir.key().as_str(),
            },
        };
        let done_before = DoneMark::of(&record.done_file);
        let adapter = record.runtime.adapter();
        let headless = Headless {
            turn,
            profile,
            model: record.model.as_deref(),
            message,
        };
        let mut command = runtime::command(&settings.program, &record.cwd);
        command
            .args(adapter.headless_args(&headless))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(append_to(&dir.agent_stderr())?)
            // The agent leads a process group, so a stop reaches what it
            // started too.
            .process_group(0);
        let agent_lock = lock.new_agent_lock()?;
        let inherited = agent_lock.as_raw_fd();
        // SAFETY: the closure runs in the child between fork and exec, where
        // only async-signal-safe calls are sound. It makes one, `fcntl`, on
        // a descriptor the child holds, and touches no shared memory.
        //
        // Clearing close-on-exec hands the agent lock to the agent. The
        // lock is then held for as long as the agent, or a process it
        // started, lives.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(inherited, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let watch = adapter.watch(&headless);
        let spawned = command.spawn();
        // The holder's own copy goes, so only the agent's side holds the
        // lock from here on.
        drop(agent_lock);
        let child = match spawned {
            Ok(child) => child,
            Err(e) => {
                let reason = format!("cannot start {}: {e}", settings.program.display());
                lock.write_state(&Snapshot::now(
                    run,
                    State::Failed {
                        reason: reason.clone(),
                    },
                    Vec::new(),
                ))?;
                return Err(reason);
            }
        };
        let running = State::Running {
            holder_pid: process::id(),
            agent_pid: child.id(),
        };
        let mut run = Self {
            child,
            watch,
            record,
            run,
            done_before,
        };
        if let Err(e) = lock.write_state(&Snapshot::now(run.run, running, Vec::new())) {
            // An agent that no state names is one nobody could stop. The
            // run ends here.
            let _ = run.child.kill();
            let _ = run.child.wait();
            return Err(e);
        }
        Ok(run)
    }

    /// Read the agent's output until it exits, then record how the run
    /// ended.
    fn follow(mut self, lock: &WriterLock) -> Result<(), String> {
        let dir = lock.dir();
        let mut problems = Problems::default();
        let mut log = match append_to(&dir.stream_log()) {
            Ok(log) => Some(log),
            Err(e) => {
                problems.add(e);
                None
            }
        };
        if let Some(stdout) = self.child.stdout.take() {
            let mut reader = BufReader::new(stdout);
            let mut bytes = Vec::new();
            loop {
                bytes.clear();
                match reader.read_until(b'\n', &mut bytes) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => {
                        // Dropping the reader closes the pipe, so the agent
                        // cannot block on a full one while the holder waits.
                        problems.add(format!("cannot read the agent's output: {e}"));
                        break;
                    }
                }
                if let Some(file) = &mut log
                    && let Err(e) = file.write_all(&bytes)
                {
                    problems.add(format!("cannot write {}: {e}", dir.stream_log().display()));
                    log = None;
                }
                let text = String::from_utf8_lossy(&bytes);
                let line = text.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    continue;
                }
                let seen = match self.watch.line(line) {
                    Ok(seen) => seen,
                    Err(e) => {
                        problems.add(e);
                        continue;
                    }
                };
                if let Some(problem) = seen.problem {
                    problems.add(problem);
                }
                if let Some(id) = seen.session_id
                    && self.record.session_id.as_deref() != Some(id.as_str())
                {
                    self.record.session_id = Some(id);
                    if let Err(e) = lock.write_record(&self.record) {
                        problems.add(format!("cannot record the session id: {e}"));
                    }
                }
            }
        }
        let ended = match self.child.wait() {
            Ok(exit) => self.watch.end(exit, self.record.session_id.as_deref()),
            Err(e) => Ended::Failed(format!("lost the agent process: {e}")),
        };
        let state = settle(
            dir.stop_requested(self.run),
            self.done_before.written_since(&self.record.done_file),
            ended,
        );
        lock.write_state(&Snapshot::now(self.run, state, problems.into_list()))
    }
}

/// The state a headless run ended in. A stop outranks the done file, and
/// the done file outranks whatever the runtime says: the caller asked for
/// the one and set the other as the sign of done.
fn settle(stop_requested: bool, done: bool, ended: Ended) -> State {
    if stop_requested {
        return State::Stopped;
    }
    if done {
        return State::Done;
    }
    match ended {
        Ended::Completed => State::Question,
        Ended::Failed(reason) => State::Failed { reason },
        Ended::Limited { resets_at } => State::Limited { resets_at },
    }
}

/// Open the session in `dir` interactively: run the agent in this terminal,
/// in the session's directory, and hold the writer lock until it exits.
///
/// An open runs no permission profile. A person answers the agent's
/// prompts, so the runtime's own defaults apply.
pub fn open(home: &Home, dir: &SessionDir) -> Result<Snapshot, String> {
    let lock = match dir.acquire()? {
        Acquired::Held(lock) => lock,
        Acquired::Busy => return Err(busy(dir.key())),
    };
    let run = lock.next_run()?;
    let record = dir.read_record()?;
    let session_id = record.session_id.as_deref().ok_or_else(|| {
        format!(
            "{:?} has no session id to open: its first run never reported one",
            dir.key().as_str()
        )
    })?;
    let config = Config::load(home)?;
    let program = &config.runtime(record.runtime).program;
    let done_before = DoneMark::of(&record.done_file);
    let mut command = runtime::command(program, &record.cwd);
    command.args(record.runtime.adapter().interactive_args(session_id));
    lock.write_state(&Snapshot::now(
        run,
        State::Open {
            holder_pid: process::id(),
        },
        Vec::new(),
    ))?;
    let state = match command.status() {
        Ok(_) if done_before.written_since(&record.done_file) => State::Done,
        Ok(_) => State::Closed,
        Err(e) => State::Failed {
            reason: format!("cannot start {}: {e}", program.display()),
        },
    };
    let snapshot = Snapshot::now(run, state, Vec::new());
    lock.write_state(&snapshot)?;
    Ok(snapshot)
}

/// When the done file was last written, or `None` while it does not exist.
/// A run counts as done when the file exists after it and was not the same
/// file before it, so a done file from an earlier run does not count.
#[derive(Debug, PartialEq, Eq)]
struct DoneMark(Option<SystemTime>);

impl DoneMark {
    fn of(path: &Path) -> Self {
        Self(fs::metadata(path).and_then(|meta| meta.modified()).ok())
    }

    fn written_since(&self, path: &Path) -> bool {
        let now = Self::of(path);
        now.0.is_some() && now != *self
    }
}

/// The problems of one run, up to [`MAX_PROBLEMS`], and a count of the rest.
#[derive(Debug, Default)]
struct Problems {
    list: Vec<String>,
    more: usize,
}

impl Problems {
    fn add(&mut self, problem: String) {
        if self.list.len() < MAX_PROBLEMS {
            self.list.push(problem);
        } else {
            self.more += 1;
        }
    }

    fn into_list(mut self) -> Vec<String> {
        if self.more > 0 {
            self.list.push(format!("{} more problems", self.more));
        }
        self.list
    }
}

fn append_to(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Each stop is its own state, and the order between them holds.
    #[test]
    fn each_ending_settles_into_its_own_state() {
        let failed = || Ended::Failed("x".to_string());
        assert_eq!(settle(true, true, Ended::Completed), State::Stopped);
        assert_eq!(settle(false, true, failed()), State::Done);
        assert_eq!(
            settle(false, true, Ended::Limited { resets_at: None }),
            State::Done
        );
        assert_eq!(settle(false, false, Ended::Completed), State::Question);
        assert_eq!(
            settle(false, false, failed()),
            State::Failed {
                reason: "x".to_string()
            }
        );
        assert_eq!(
            settle(false, false, Ended::Limited { resets_at: Some(9) }),
            State::Limited { resets_at: Some(9) }
        );
    }

    /// A done file that existed before the run, untouched, is not a sign
    /// that this run is done.
    #[test]
    fn only_a_done_file_written_during_the_run_counts() {
        let path = env::temp_dir().join(format!("heinzel-done-{}", process::id()));
        let _ = fs::remove_file(&path);
        let absent = DoneMark::of(&path);
        assert!(!absent.written_since(&path));
        fs::write(&path, "done").unwrap();
        assert!(absent.written_since(&path));
        let present = DoneMark::of(&path);
        assert!(!present.written_since(&path));
        let later = SystemTime::now() + Duration::from_secs(5);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(present.written_since(&path));
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn problems_past_the_cap_are_counted() {
        let mut problems = Problems::default();
        for n in 0..MAX_PROBLEMS + 3 {
            problems.add(n.to_string());
        }
        let list = problems.into_list();
        assert_eq!(list.len(), MAX_PROBLEMS + 1);
        assert_eq!(list[MAX_PROBLEMS], "3 more problems");
    }
}
