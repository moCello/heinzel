//! One agent turn, run to its end for a caller that waits for it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use heinzel_runtime::{self as runtime, Ended, Headless, Problems, RuntimeName};

use crate::group::Group;

/// The agent CLI a turn runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Claude,
    Codex,
}

impl Runtime {
    fn name(self) -> RuntimeName {
        match self {
            Runtime::Claude => RuntimeName::Claude,
            Runtime::Codex => RuntimeName::Codex,
        }
    }
}

/// The agent session a turn runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    /// Open a new session. `id` is the id the session must get, or `None`
    /// to let the runtime choose. claude takes an id. codex chooses its own,
    /// and a turn that gives it one is refused.
    New { id: Option<String> },
    /// Continue the session with the id `id`.
    Resume { id: String },
}

/// One turn: what the agent runs, where, and for how long.
#[derive(Debug, Clone)]
pub struct Turn {
    pub runtime: Runtime,
    /// The program to run. A bare name is looked up on `PATH`.
    pub program: PathBuf,
    /// The directory the agent runs in. It must exist.
    pub cwd: PathBuf,
    pub session: Session,
    /// The model, or `None` for the CLI's default.
    pub model: Option<String>,
    /// The arguments that set what the agent may do, and any other flag
    /// the caller needs on the command line. heinzel puts them there as
    /// given and adds no permission of its own.
    pub args: Vec<String>,
    /// How long the turn may run. It must be above zero. At the limit,
    /// heinzel sends `SIGTERM` to the turn's process group, and `SIGKILL`
    /// [`crate::TERM_GRACE`] later.
    ///
    /// A turn also stops when the caller's process dies, for any reason: a
    /// Ctrl-C, a `SIGKILL`, a crash. A guard process then sends the same
    /// signals to the group. The group does not hold a process that left
    /// it through `setsid` or `setpgid`.
    pub time_limit: Duration,
    /// The message the agent gets.
    pub message: String,
}

/// How a turn ended, and what went wrong on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    /// What went wrong without ending the turn: a line of output heinzel
    /// could not read, or a permission mode claude applied in place of the
    /// one the arguments asked for.
    pub problems: Vec<String>,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The agent finished its turn in the session `session_id`. `answer` is
    /// its final message, or `None` when its output carried none.
    Finished {
        session_id: String,
        answer: Option<String>,
    },
    /// The account hit a usage limit. It resets at `resets_at`, in seconds
    /// since the Unix epoch, when the runtime said.
    Limited { resets_at: Option<i64> },
    /// The turn failed, for the reason given. A turn that ran past its time
    /// limit failed.
    Failed { reason: String },
}

/// Run `turn` and wait for its end.
///
/// An `Err` is a turn that did not start: heinzel refused it, or could not
/// start the program. A turn that started ends in a [`Report`]. When heinzel
/// loses track of a started agent, it stops the agent, and the report says
/// the turn failed.
pub fn run(turn: &Turn) -> Result<Report, String> {
    if turn.time_limit.is_zero() {
        return Err("a turn needs a time limit above zero".to_string());
    }
    if !turn.cwd.is_dir() {
        return Err(format!("{} is not a directory", turn.cwd.display()));
    }
    let headless = Headless {
        turn: match &turn.session {
            Session::New { id } => runtime::Turn::Fresh {
                name: None,
                session_id: id.as_deref(),
            },
            Session::Resume { id } => runtime::Turn::Resume { session_id: id },
        },
        profile: &turn.args,
        model: turn.model.as_deref(),
        message: &turn.message,
    };
    let adapter = turn.runtime.name().adapter();
    let args = adapter.headless_args(&headless)?;
    let mut watch = adapter.watch(&headless);
    let mut command = runtime::command(&turn.program, &turn.cwd);
    command.args(args);
    let started = Instant::now();
    let deadline = started.checked_add(turn.time_limit).ok_or_else(|| {
        format!(
            "the time limit {:?} reaches past what the clock can count",
            turn.time_limit
        )
    })?;
    let group = Group::spawn(command)?;
    let mut problems = Problems::default();
    let mut seen_id: Option<String> = None;
    let followed = group.follow(deadline, &mut problems, |line, problems| {
        if let Some(id) = runtime::read_line(watch.as_mut(), line, problems) {
            seen_id = Some(id);
        }
    });
    let asked_id = match &turn.session {
        Session::New { id } => id.as_deref(),
        Session::Resume { id } => Some(id.as_str()),
    };
    if let (Some(asked), Some(seen)) = (asked_id, seen_id.as_deref())
        && asked != seen
    {
        problems.add(format!(
            "the turn asked for the session {asked:?}, and the agent named {seen:?}"
        ));
    }
    let session_id = seen_id.or(asked_id.map(str::to_string));
    let ending = match followed {
        Ok(ending) => ending,
        // The group went with `follow`, and took the agent with it.
        Err(reason) => {
            let session = session_id
                .map(|id| format!(" in the session {id:?}"))
                .unwrap_or_default();
            return Ok(Report {
                outcome: Outcome::Failed {
                    reason: format!(
                        "{reason}; heinzel lost track of the agent{session} and stopped it"
                    ),
                },
                problems: problems.into_list(),
            });
        }
    };
    let outcome = if ending.timed_out {
        Outcome::Failed {
            reason: with_stderr(
                format!(
                    "the turn ran past its time limit of {:?}, and heinzel stopped it",
                    turn.time_limit
                ),
                &ending.stderr,
            ),
        }
    } else {
        match watch.end(ending.exit, session_id.as_deref()) {
            Ended::Completed { answer } => match session_id {
                Some(session_id) => Outcome::Finished { session_id, answer },
                None => Outcome::Failed {
                    reason: "the agent finished its turn, and its output named no session"
                        .to_string(),
                },
            },
            Ended::Limited { resets_at } => Outcome::Limited { resets_at },
            Ended::Failed(reason) => Outcome::Failed {
                reason: with_stderr(reason, &ending.stderr),
            },
        }
    };
    Ok(Report {
        outcome,
        problems: problems.into_list(),
    })
}

/// `reason`, followed by the end of the agent's stderr when it wrote any.
fn with_stderr(reason: String, stderr: &str) -> String {
    if stderr.is_empty() {
        reason
    } else {
        format!("{reason}; its stderr ends: {stderr}")
    }
}

/// Why the installed `program` may not be the CLI version heinzel was
/// validated against for `runtime`, ready to print. `None` when the versions
/// agree. heinzel still runs a turn on another version.
pub fn version_note(runtime: Runtime, program: &Path) -> Option<String> {
    runtime::version_note(program, runtime.name())
}
