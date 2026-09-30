//! What a caller does to a session: start it, continue it, look at it, wait
//! for it, stop it. An open lives with the holders, in
//! [`crate::holder::open`], because it holds the writer lock itself.

use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::Config;
use crate::holder::{self, Launch};
use crate::runtime::{self, RuntimeName};
use crate::state::{Snapshot, State};
use crate::store::{Acquired, Home, Key, Record, SessionDir, WriterLock};

/// How long a stop waits for the agent to end after `SIGTERM`, before it
/// sends `SIGKILL`.
const TERM_GRACE: Duration = Duration::from_secs(10);
/// How long a stop waits for the holder to record the end after `SIGKILL`.
const KILL_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

/// What a start needs besides the key and the message.
#[derive(Debug, Clone)]
pub struct StartSpec {
    pub runtime: RuntimeName,
    /// The directory the agent runs in. It must exist.
    pub cwd: PathBuf,
    /// The file whose appearance says the work is done. An absolute path.
    pub done_file: PathBuf,
    pub profile: String,
    pub model: Option<String>,
}

/// What a caller sees of a session: its record and its state.
#[derive(Debug, Serialize)]
pub struct Report {
    pub key: String,
    #[serde(flatten)]
    pub record: Record,
    #[serde(flatten)]
    pub snapshot: Snapshot,
}

/// The report of `dir` with `snapshot` as its state.
fn report(dir: &SessionDir, snapshot: Snapshot) -> Result<Report, String> {
    Ok(Report {
        key: dir.key().as_str().to_string(),
        record: dir.read_record()?,
        snapshot,
    })
}

/// A started session, and the warning about the CLI version, if any.
pub struct Started {
    pub report: Report,
    pub version_note: Option<String>,
}

/// Start the session `key` and hand the agent `message`. A key that has a
/// session already is refused.
pub fn start(home: &Home, key: &Key, spec: StartSpec, message: &str) -> Result<Started, String> {
    // A runtime or profile the config does not know is refused before the
    // key is taken.
    let config = Config::load(home)?;
    let settings = config.runtime(spec.runtime);
    settings.profile(&spec.profile)?;
    let cwd = spec
        .cwd
        .canonicalize()
        .map_err(|e| format!("cannot use {} as the directory: {e}", spec.cwd.display()))?;
    if !cwd.is_dir() {
        return Err(format!("{} is not a directory", cwd.display()));
    }
    if !spec.done_file.is_absolute() {
        return Err(format!(
            "the done file {} is not an absolute path",
            spec.done_file.display()
        ));
    }
    let dir = home.session(key);
    dir.create(&Record {
        runtime: spec.runtime,
        cwd,
        done_file: spec.done_file,
        profile: spec.profile,
        model: spec.model,
        session_id: None,
    })?;
    let version_note = runtime::version_note(&settings.program, spec.runtime);
    launched(home, &dir, message)?;
    Ok(Started {
        report: report(&dir, dir.observe()?)?,
        version_note,
    })
}

/// Continue the session `key` headless with `message`. A session that has a
/// writer is refused. A session whose first run never reported a session id
/// gets a fresh run instead, so a caller can retry the key.
pub fn resume(home: &Home, key: &Key, message: &str) -> Result<Report, String> {
    let dir = home.session(key);
    dir.require()?;
    launched(home, &dir, message)?;
    report(&dir, dir.observe()?)
}

fn launched(home: &Home, dir: &SessionDir, message: &str) -> Result<(), String> {
    match holder::launch(home, dir, message)? {
        Launch::Started => Ok(()),
        Launch::Busy => Err(holder::busy(dir.key())),
    }
}

/// Open the session `key` in this terminal.
pub fn open(home: &Home, key: &Key) -> Result<Report, String> {
    let dir = home.session(key);
    dir.require()?;
    let snapshot = holder::open(home, &dir)?;
    report(&dir, snapshot)
}

/// The session `key` as it stands.
pub fn status(home: &Home, key: &Key) -> Result<Report, String> {
    let dir = home.session(key);
    report(&dir, dir.observe()?)
}

/// Wait until the session `key` has no writer, and return how it stands.
pub fn wait(home: &Home, key: &Key) -> Result<Report, String> {
    let dir = home.session(key);
    report(&dir, dir.wait()?)
}

/// Stop the headless run of the session `key`.
///
/// While a holder runs, the stop asks it to record the run as stopped, then
/// signals the agent's process group: `SIGTERM`, and `SIGKILL` when the
/// agent outlasts [`TERM_GRACE`]. The holder records the end, as it does for
/// every run.
///
/// A live state with no holder is what a lost holder leaves. The stop then
/// ends the agent that the state names and records the stop itself.
pub fn stop(home: &Home, key: &Key) -> Result<Report, String> {
    let dir = home.session(key);
    match dir.acquire()? {
        Acquired::Held(lock) => stop_without_holder(&dir, &lock),
        Acquired::Busy => stop_through_holder(&dir),
    }
}

fn stop_through_holder(dir: &SessionDir) -> Result<Report, String> {
    let key = dir.key().as_str();
    let snapshot = dir.observe()?;
    let (run, agent_pid) = match snapshot.state {
        State::Running { agent_pid, .. } => (snapshot.run, agent_pid),
        State::Open { .. } => return Err(format!("{key:?} is open in a terminal: end it there")),
        state => return Err(not_running(key, &state)),
    };
    dir.request_stop(run)?;
    if let Err(e) = signal_group(agent_pid, libc::SIGTERM) {
        dir.withdraw_stop()?;
        return Err(e);
    }
    if let Some(ended) = ended_after(dir, run, TERM_GRACE)? {
        return report(dir, ended);
    }
    signal_group(agent_pid, libc::SIGKILL)?;
    match ended_after(dir, run, KILL_GRACE)? {
        Some(ended) => report(dir, ended),
        None => Err(format!(
            "the agent of {key:?} got SIGKILL, and its holder has not recorded the end after {}s",
            KILL_GRACE.as_secs()
        )),
    }
}

fn stop_without_holder(dir: &SessionDir, lock: &WriterLock) -> Result<Report, String> {
    let key = dir.key().as_str();
    let snapshot = lock
        .read_state()?
        .ok_or_else(|| format!("{key:?} has no state yet"))?;
    match snapshot.state {
        // The agent lock says whether the recorded group is still the
        // agent's. Once the agent is gone, the id may name someone else's
        // group, which a signal must not reach. No holder waits for the
        // agent's end, so a grace period would only delay the same outcome.
        State::Running { agent_pid, .. } => {
            if dir.agent_alive()? {
                signal_group(agent_pid, libc::SIGKILL)?;
            }
        }
        // The agent of an open ran in the terminal of the process that
        // held it, and no state names it.
        State::Open { .. } => {}
        state => return Err(not_running(key, &state)),
    }
    let stopped = Snapshot::now(snapshot.run, State::Stopped, snapshot.problems);
    lock.write_state(&stopped)?;
    report(dir, stopped)
}

fn not_running(key: &str, state: &State) -> String {
    format!(
        "{key:?} is not running: its state is {}",
        serde_json::to_string(state).unwrap_or_default()
    )
}

/// The state once run `run` of `dir` has ended, or `None` when it has not
/// ended within `limit`. A later run counts as the end of this one.
fn ended_after(dir: &SessionDir, run: u32, limit: Duration) -> Result<Option<Snapshot>, String> {
    let deadline = Instant::now() + limit;
    loop {
        let snapshot = dir.observe()?;
        if snapshot.run != run || !snapshot.state.is_live() {
            return Ok(Some(snapshot));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL);
    }
}

/// Send `signal` to the process group that `leader` leads. A group that is
/// gone already counts as reached.
fn signal_group(leader: u32, signal: libc::c_int) -> Result<(), String> {
    let group = i32::try_from(leader)
        .map_err(|_| format!("the agent's process id {leader} is out of range"))?;
    // SAFETY: `kill` takes plain integers and touches no memory.
    if unsafe { libc::kill(-group, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(format!(
        "cannot signal the agent's process group {group}: {error}"
    ))
}
