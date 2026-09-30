//! The state of a session.
//!
//! One process writes it: the one that holds the session's writer lock. That
//! is the detached holder of a headless run, or the foreground process of an
//! interactive open. Nothing else decides whether a session runs. A reader
//! that finds a live state and a free lock reports a lost holder instead: see
//! [`crate::store::SessionDir::observe`].

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// What the state file of a session holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The run this state belongs to. The first run of a session is 1, and
    /// each continue and each open counts one more.
    pub run: u32,
    /// When the session entered this state, in seconds since the Unix epoch.
    pub since: u64,
    #[serde(flatten)]
    pub state: State,
    /// Problems the holder met during the run that did not stop it: a line
    /// of the agent's output that the adapter could not read, or a session id
    /// the holder could not record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

impl Snapshot {
    /// `state` as of now, for `run`.
    pub fn now(run: u32, state: State, problems: Vec<String>) -> Self {
        Self {
            run,
            since: unix_now(),
            state,
            problems,
        }
    }
}

/// Where a session stands. Each way a run can stop is its own variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    /// A headless run is in progress.
    Running {
        /// The detached holder that owns the run.
        holder_pid: u32,
        /// The agent process. It leads its own process group, so a stop
        /// signals the whole group through this id.
        agent_pid: u32,
    },
    /// An interactive open is in progress.
    Open {
        /// The foreground process that runs the open.
        holder_pid: u32,
    },
    /// The agent wrote the done file.
    Done,
    /// The agent finished its turn without the done file. It stopped to ask
    /// something, or it gave up without saying so.
    Question,
    /// The run failed.
    Failed { reason: String },
    /// The account hit a usage limit. A continue before the reset hits it
    /// again.
    Limited {
        /// When the limit resets, in seconds since the Unix epoch, when the
        /// runtime said.
        resets_at: Option<i64>,
    },
    /// A stop ended the run.
    Stopped,
    /// An interactive open ended without the done file.
    Closed,
}

impl State {
    /// Whether a process that holds the writer lock is still in this state.
    pub fn is_live(&self) -> bool {
        matches!(self, State::Running { .. } | State::Open { .. })
    }
}

/// The current time in seconds since the Unix epoch. A clock set before the
/// epoch reads as 0.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A caller reads the state file as JSON, so its shape is part of the
    /// surface: the variant name sits under `state`, beside its fields.
    #[test]
    fn a_snapshot_reads_as_flat_json() {
        let snapshot = Snapshot {
            run: 2,
            since: 7,
            state: State::Limited {
                resets_at: Some(1_800_000_000),
            },
            problems: Vec::new(),
        };
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"run": 2, "since": 7, "state": "limited", "resets_at": 1_800_000_000})
        );
        assert_eq!(serde_json::from_value::<Snapshot>(json).unwrap(), snapshot);
    }
}
