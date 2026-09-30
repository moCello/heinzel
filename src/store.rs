//! What heinzel keeps on disk about each session.
//!
//! Each key has its own directory under the home. It holds:
//!
//! - `record.json`: what the session is. The start writes it, and the holder
//!   adds the session id once the agent reports it.
//! - `state.json`: where the session stands. Only a lock holder writes it.
//! - `lock`: the writer lock. A holder keeps it for the life of its run.
//! - `agent.lock`: the agent lock. Each run makes a new one, and only its
//!   agent and the processes the agent starts hold it. While it is held,
//!   the agent of the last run lives.
//! - `stream.jsonl`, `agent.stderr`: what the agent printed, run after run.
//! - `holder.log`: what the holder itself printed.
//! - `stop-request`: the number of the run that a stop asked to end. The
//!   file stays after that run. A later run ignores it, since it names
//!   another run.

use std::env;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::runtime::RuntimeName;
use crate::state::Snapshot;

/// The variable that names the home directory. Without it the home is
/// `~/.heinzel`.
pub const HOME_VAR: &str = "HEINZEL_HOME";

/// How often a writer tries the lock before it reads as busy, and how long it
/// waits between tries. A reader takes the lock shared for as long as one
/// file read, so a writer that meets a reader waits it out. A writer that
/// meets a holder does not: a holder keeps the lock for minutes.
const LOCK_TRIES: u32 = 50;
const LOCK_PAUSE: Duration = Duration::from_millis(10);

/// The directory that holds every session and the config file.
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// The home that [`HOME_VAR`] names, or `~/.heinzel`.
    pub fn from_env() -> Result<Self, String> {
        if let Some(root) = env::var_os(HOME_VAR) {
            return Ok(Self { root: root.into() });
        }
        let home =
            env::var_os("HOME").ok_or_else(|| format!("neither {HOME_VAR} nor HOME is set"))?;
        Ok(Self {
            root: PathBuf::from(home).join(".heinzel"),
        })
    }

    /// The home at `root`. Tests point it at a scratch directory.
    #[cfg(test)]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// The directory of the session `key`, which need not exist.
    pub fn session(&self, key: &Key) -> SessionDir {
        SessionDir {
            key: key.clone(),
            dir: self.root.join("sessions").join(key.as_str()),
        }
    }
}

/// A caller's name for a session. heinzel gives it no meaning. It becomes a
/// directory name, so it holds only ASCII letters, digits, `.`, `_` and `-`,
/// and it does not start with a `.`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key(String);

impl Key {
    pub fn parse(text: &str) -> Result<Self, String> {
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        if text.is_empty() || text.len() > 128 {
            return Err(format!(
                "a key holds 1 to 128 characters, and {text:?} holds {}",
                text.len()
            ));
        }
        if text.starts_with('.') || !text.chars().all(allowed) {
            return Err(format!(
                "a key holds only ASCII letters, digits, `.`, `_` and `-`, and does not start \
with `.`: {text:?}"
            ));
        }
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a session is. The start writes it once. The holder adds the session
/// id when the agent reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub runtime: RuntimeName,
    /// Where the agent runs, and where an open runs it.
    pub cwd: PathBuf,
    /// The file whose appearance says the work is done.
    pub done_file: PathBuf,
    /// The permission profile every headless run of the session uses.
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The runtime's id for the session. It is unknown until the agent
    /// reports it.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// The directory of one session.
#[derive(Debug, Clone)]
pub struct SessionDir {
    key: Key,
    dir: PathBuf,
}

/// The outcome of a try for the writer lock.
pub enum Acquired {
    Held(WriterLock),
    /// Another writer holds the lock.
    Busy,
}

/// The writer lock of a session. Every write to the session's state goes
/// through it, so only the lock holder can write. Dropping it releases the
/// lock.
pub struct WriterLock {
    file: File,
    dir: SessionDir,
}

impl WriterLock {
    pub fn dir(&self) -> &SessionDir {
        &self.dir
    }

    /// The state the last writer left, or `None` before the first run
    /// recorded one. A lock holder reads it here: see
    /// [`SessionDir::observe`] for everyone else.
    pub fn read_state(&self) -> Result<Option<Snapshot>, String> {
        if self.dir.state_path().exists() {
            self.dir.read_state().map(Some)
        } else {
            Ok(None)
        }
    }

    /// The number of the run a new writer starts.
    ///
    /// A live state under this lock means the last holder ended without a
    /// word, and its agent may still run. A new run is refused until a stop
    /// records the end of the old one.
    pub fn next_run(&self) -> Result<u32, String> {
        match self.read_state()? {
            None => Ok(1),
            Some(previous) if previous.state.is_live() => Err(self.dir.lost_holder(&previous)),
            Some(previous) => Ok(previous.run + 1),
        }
    }

    pub fn write_state(&self, snapshot: &Snapshot) -> Result<(), String> {
        write_json(&self.dir.state_path(), snapshot)
    }

    /// A new agent lock for the next run, locked. The holder hands it to
    /// the agent: see [`SessionDir::agent_alive`].
    ///
    /// The old file is removed first, so a process of an earlier run that
    /// still holds it holds a file no one looks at.
    pub fn new_agent_lock(&self) -> Result<File, String> {
        let path = self.dir.agent_lock_path();
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {}: {e}", path.display())),
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        file.try_lock()
            .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
        Ok(file)
    }

    pub fn write_record(&self, record: &Record) -> Result<(), String> {
        write_json(&self.dir.record_path(), record)
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // Closing the file would release the lock as well. The explicit
        // unlock only says so. An error here leaves the lock to the close.
        let _ = self.file.unlock();
    }
}

impl SessionDir {
    pub fn key(&self) -> &Key {
        &self.key
    }

    pub fn stream_log(&self) -> PathBuf {
        self.dir.join("stream.jsonl")
    }

    pub fn agent_stderr(&self) -> PathBuf {
        self.dir.join("agent.stderr")
    }

    pub fn holder_log(&self) -> PathBuf {
        self.dir.join("holder.log")
    }

    fn record_path(&self) -> PathBuf {
        self.dir.join("record.json")
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("lock")
    }

    fn agent_lock_path(&self) -> PathBuf {
        self.dir.join("agent.lock")
    }

    /// Whether the agent of the last run, or a process it started, still
    /// holds the agent lock.
    ///
    /// A process id alone cannot say this: once the agent is gone, the
    /// system may give its id to another process.
    pub fn agent_alive(&self) -> Result<bool, String> {
        let path = self.agent_lock_path();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(format!("cannot open {}: {e}", path.display())),
        };
        match file.try_lock_shared() {
            Ok(()) => {
                let _ = file.unlock();
                Ok(false)
            }
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
        }
    }

    fn stop_request_path(&self) -> PathBuf {
        self.dir.join("stop-request")
    }

    /// Make the directory and write `record` into it. A key that already has
    /// a directory is refused, so a start never takes over a session.
    pub fn create(&self, record: &Record) -> Result<(), String> {
        if let Some(parent) = self.dir.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        match fs::create_dir(&self.dir) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                return Err(format!(
                    "a session with the key {:?} exists already",
                    self.key.as_str()
                ));
            }
            Err(e) => return Err(format!("cannot create {}: {e}", self.dir.display())),
        }
        // No holder can exist yet: the directory it would lock in is new.
        write_json(&self.record_path(), record)
    }

    /// Fail unless the key has a session.
    pub fn require(&self) -> Result<(), String> {
        if self.dir.is_dir() {
            Ok(())
        } else {
            Err(format!("no session has the key {:?}", self.key.as_str()))
        }
    }

    pub fn read_record(&self) -> Result<Record, String> {
        read_json(&self.record_path())
    }

    fn read_state(&self) -> Result<Snapshot, String> {
        read_json(&self.state_path())
    }

    /// Try for the writer lock. A holder takes it for its run, and an open
    /// takes it for the open.
    pub fn acquire(&self) -> Result<Acquired, String> {
        let file = self.open_lock()?;
        for _ in 0..LOCK_TRIES {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(Acquired::Held(WriterLock {
                        file,
                        dir: self.clone(),
                    }));
                }
                Err(TryLockError::WouldBlock) => thread::sleep(LOCK_PAUSE),
                Err(TryLockError::Error(e)) => return Err(self.lock_error(&e)),
            }
        }
        Ok(Acquired::Busy)
    }

    /// The state of the session, as a reader sees it.
    ///
    /// While a writer holds the lock, the state file says what the writer
    /// wrote. With the lock free, a live state means the writer ended
    /// without a word. That is an error, not a state: the agent it ran may
    /// still be alive.
    pub fn observe(&self) -> Result<Snapshot, String> {
        let file = self.open_lock()?;
        match file.try_lock_shared() {
            Ok(()) => {
                let snapshot = self.read_state();
                let _ = file.unlock();
                self.settled(snapshot?)
            }
            Err(TryLockError::WouldBlock) => self.read_state(),
            Err(TryLockError::Error(e)) => Err(self.lock_error(&e)),
        }
    }

    /// Block until no writer holds the lock, then return the state.
    pub fn wait(&self) -> Result<Snapshot, String> {
        let file = self.open_lock()?;
        file.lock_shared().map_err(|e| self.lock_error(&e))?;
        let snapshot = self.read_state();
        let _ = file.unlock();
        self.settled(snapshot?)
    }

    /// The error for a live `snapshot` under a free lock.
    fn lost_holder(&self, snapshot: &Snapshot) -> String {
        format!(
            "the holder of {:?} ended without recording a stop, and the state still says {}: see \
{}. `heinzel stop` ends the agent it names and records the stop",
            self.key.as_str(),
            describe_live(snapshot),
            self.holder_log().display()
        )
    }

    /// `snapshot`, read under a free lock.
    fn settled(&self, snapshot: Snapshot) -> Result<Snapshot, String> {
        if snapshot.state.is_live() {
            return Err(self.lost_holder(&snapshot));
        }
        Ok(snapshot)
    }

    /// Ask the holder of run `run` to record the run as stopped when its
    /// agent ends.
    pub fn request_stop(&self, run: u32) -> Result<(), String> {
        let path = self.stop_request_path();
        fs::write(&path, run.to_string())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Take back a stop request that could not reach its run.
    pub fn withdraw_stop(&self) -> Result<(), String> {
        let path = self.stop_request_path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
        }
    }

    /// Whether a stop asked to end run `run`.
    pub fn stop_requested(&self, run: u32) -> bool {
        fs::read_to_string(self.stop_request_path())
            .is_ok_and(|named| named.trim() == run.to_string())
    }

    fn open_lock(&self) -> Result<File, String> {
        self.require()?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.lock_path())
            .map_err(|e| format!("cannot open {}: {e}", self.lock_path().display()))
    }

    fn lock_error(&self, error: &io::Error) -> String {
        format!("cannot lock {}: {error}", self.lock_path().display())
    }
}

fn describe_live(snapshot: &Snapshot) -> String {
    serde_json::to_string(&snapshot.state).unwrap_or_else(|_| "a live state".to_string())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let text = fs::read_to_string(path).map_err(|e| match e.kind() {
        ErrorKind::NotFound => format!("{} does not exist", path.display()),
        _ => format!("cannot read {}: {e}", path.display()),
    })?;
    serde_json::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

/// Write `value` to `path` in one step: a reader sees the old file or the new
/// one, never a part.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|e| format!("cannot encode {}: {e}", path.display()))?;
    text.push('\n');
    let staged = path.with_extension("json.new");
    fs::write(&staged, text).map_err(|e| format!("cannot write {}: {e}", staged.display()))?;
    fs::rename(&staged, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key becomes a directory name, so a key that could leave the
    /// sessions directory is refused.
    #[test]
    fn a_key_cannot_name_a_path_outside_its_directory() {
        for bad in ["", "..", ".hidden", "a/b", "a b", "ä", &"k".repeat(129)] {
            assert!(Key::parse(bad).is_err(), "{bad:?} was accepted");
        }
        for good in ["7", "job-1.try_2", &"k".repeat(128)] {
            assert_eq!(Key::parse(good).unwrap().as_str(), good);
        }
    }

    /// A stop request that a run outlived does not stop the next run.
    #[test]
    fn a_stop_request_reaches_only_the_run_it_names() {
        let root = env::temp_dir().join(format!("heinzel-store-stop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = Home::at(&root).session(&Key::parse("job").unwrap());
        fs::create_dir_all(&dir.dir).unwrap();
        assert!(!dir.stop_requested(3));
        dir.request_stop(3).unwrap();
        assert!(dir.stop_requested(3));
        assert!(!dir.stop_requested(4));
        dir.withdraw_stop().unwrap();
        assert!(!dir.stop_requested(3));
        fs::remove_dir_all(&root).unwrap();
    }
}
