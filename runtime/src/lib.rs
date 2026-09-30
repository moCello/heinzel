//! One adapter per agent CLI.
//!
//! An adapter builds the command line of a headless run and of an open. It
//! reads the session id from the run's output, and it says how the run ended.
//! It names the CLI version it was validated against. Everything else about a
//! session is the same for every runtime, and lives outside this crate.
//!
//! The heinzel binary and the heinzel library both run these adapters. The
//! library does not export them: its callers run a whole turn, not a stage.

mod claude;
mod codex;

use std::path::Path;
use std::process::{Command, ExitStatus};

use serde::{Deserialize, Serialize};

/// The environment an agent session gives the processes it runs, and that a
/// new agent must not take for its own. A headless start from inside a claude
/// session would otherwise take itself for that session.
const PARENT_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_PID",
];

/// The runtimes heinzel knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeName {
    Claude,
    Codex,
}

impl RuntimeName {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            _ => Err(format!(
                "no runtime is named {text:?}; the runtimes are claude, codex"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn adapter(self) -> &'static dyn Runtime {
        match self {
            Self::Claude => &claude::Claude,
            Self::Codex => &codex::Codex,
        }
    }
}

/// How a headless run relates to the agent's session.
#[derive(Debug, Clone, Copy)]
pub enum Turn<'a> {
    /// Open a new session. `name` is the caller's name for it, which a
    /// runtime may show as the session's name. `session_id` is the id the
    /// session must get, for a runtime that lets its caller choose one.
    Fresh {
        name: Option<&'a str>,
        session_id: Option<&'a str>,
    },
    /// Continue the session with this id.
    Resume { session_id: &'a str },
}

/// Everything a headless run puts on the command line.
#[derive(Debug, Clone, Copy)]
pub struct Headless<'a> {
    pub turn: Turn<'a>,
    /// The permission arguments: those of a profile, or those a caller of
    /// the library gives. The run puts them on the command line as given.
    pub profile: &'a [String],
    pub model: Option<&'a str>,
    pub message: &'a str,
}

/// How a headless run ended, as far as the runtime can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The agent finished its turn. `answer` is its final message, when
    /// the output carried one.
    Completed { answer: Option<String> },
    /// The run failed, for the reason given.
    Failed(String),
    /// The account hit a usage limit. It resets at this time, in seconds
    /// since the Unix epoch, when the runtime said.
    Limited { resets_at: Option<i64> },
}

/// One agent CLI.
pub trait Runtime: Sync {
    /// The CLI version this adapter was validated against: the last one
    /// `make boundary` passed on.
    fn validated_version(&self) -> &'static str;

    /// The arguments of a headless run. A run this CLI cannot do as asked is
    /// an error.
    fn headless_args(&self, run: &Headless<'_>) -> Result<Vec<String>, String>;

    /// The arguments of an interactive open of the session `session_id`.
    fn interactive_args(&self, session_id: &str) -> Vec<String>;

    /// A reader for the output of `run`.
    fn watch(&self, run: &Headless<'_>) -> Box<dyn Watch>;
}

/// What one line of a run's output says.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// The session id, when the line names it.
    pub session_id: Option<String>,
    /// Something about the run the caller must see that does not end it:
    /// for example, a permission mode the CLI applied in place of the one
    /// the profile asked for.
    pub problem: Option<String>,
}

/// Reads the output of one headless run, line by line.
pub trait Watch {
    /// Read one line. A line the adapter cannot read is an error.
    fn line(&mut self, line: &str) -> Result<Seen, String>;

    /// How the run ended, now that the agent exited with `exit`.
    /// `session_id` is the session the run belonged to, when it is known.
    fn end(self: Box<Self>, exit: ExitStatus, session_id: Option<&str>) -> Ended;
}

/// The most problems a run records. A stream that is not what the adapter
/// expects may hold a problem on every line.
const MAX_PROBLEMS: usize = 20;

/// The problems of one run, up to [`MAX_PROBLEMS`], and a count of the rest.
#[derive(Debug, Default)]
pub struct Problems {
    list: Vec<String>,
    more: usize,
}

impl Problems {
    pub fn add(&mut self, problem: String) {
        if self.list.len() < MAX_PROBLEMS {
            self.list.push(problem);
        } else {
            self.more += 1;
        }
    }

    pub fn into_list(mut self) -> Vec<String> {
        if self.more > 0 {
            self.list.push(format!("{} more problems", self.more));
        }
        self.list
    }
}

/// Hand `watch` one line of a run's output: `bytes`, with or without its
/// line end. Returns the session id when the line names one. What the line
/// shows wrong goes to `problems`.
pub fn read_line(watch: &mut dyn Watch, bytes: &[u8], problems: &mut Problems) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let line = text.trim_end_matches(['\n', '\r']);
    if line.is_empty() {
        return None;
    }
    match watch.line(line) {
        Ok(seen) => {
            if let Some(problem) = seen.problem {
                problems.add(problem);
            }
            seen.session_id
        }
        Err(e) => {
            problems.add(e);
            None
        }
    }
}

/// A command for `program` that runs in `cwd`, without the environment of a
/// parent agent session.
pub fn command(program: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(cwd);
    for var in PARENT_SESSION_VARS {
        command.env_remove(var);
    }
    command
}

/// Why the installed CLI may not be the one the adapter was validated
/// against, ready to print. `None` when the versions agree.
///
/// An adapter rests on facts about a CLI heinzel does not own: its flags and
/// its output. A newer CLI can change either, so a run on another version
/// says so. It never refuses.
pub fn version_note(program: &Path, runtime: RuntimeName) -> Option<String> {
    let validated = runtime.adapter().validated_version();
    let name = runtime.as_str();
    match installed_version(program) {
        Ok(installed) if installed == validated => None,
        Ok(installed) => Some(format!(
            "`{} --version` says {installed:?}, and the {name} adapter was validated against \
{validated}: its flags or its output may have changed",
            program.display()
        )),
        Err(e) => Some(format!(
            "{e}: the {name} adapter was validated against {validated}"
        )),
    }
}

/// The version number that `program --version` prints. The error says why
/// there is none.
pub fn installed_version(program: &Path) -> Result<String, String> {
    let shown = program.display();
    let output = Command::new(program)
        .arg("--version")
        .output()
        .map_err(|e| format!("cannot run `{shown} --version` ({e})"))?;
    if !output.status.success() {
        return Err(format!("`{shown} --version` failed ({})", output.status));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    version_number(&text).map(String::from).ok_or_else(|| {
        format!(
            "`{shown} --version` says {:?}, which holds no version number",
            text.trim()
        )
    })
}

/// The version number in a CLI's `--version` output: the first word that
/// starts with a digit and holds a dot. claude prints `2.1.285 (Claude Code)`,
/// and codex prints `codex-cli 0.159.0`.
fn version_number(output: &str) -> Option<&str> {
    output
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()) && word.contains('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_read_out_of_each_clis_output() {
        assert_eq!(version_number("2.1.285 (Claude Code)\n"), Some("2.1.285"));
        assert_eq!(version_number("codex-cli 0.159.0\n"), Some("0.159.0"));
        assert_eq!(version_number("no version here\n"), None);
    }

    #[test]
    fn each_validated_version_is_a_version_number() {
        for runtime in [RuntimeName::Claude, RuntimeName::Codex] {
            let validated = runtime.adapter().validated_version();
            assert_eq!(version_number(validated), Some(validated), "{runtime:?}");
        }
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
