//! The codex adapter.
//!
//! A headless run is `codex exec --json`, and a continue is
//! `codex exec resume --json <id>`. codex chooses the session id and names
//! it in the `thread.started` event. It exits after its turn, and the turn
//! finished only if a `turn.completed` event says so: codex exits 0 when the
//! agent's own actions fail. The text of the last `agent_message` item is
//! the agent's answer. A new session cannot take an id its caller chose.
//!
//! A failed turn carries only `{ message }` in the `--json` stream, so the
//! stream cannot tell a usage limit from any other failure. The session file
//! codex keeps under `$CODEX_HOME/sessions` can. Before codex fails a turn on
//! a usage limit, it records the account's rate limits in a `token_count`
//! event there: each window with its `used_percent` and its `resets_at`, in
//! seconds since the Unix epoch. A failed turn whose last such record shows a
//! full window, or names a reached limit, is a usage limit.

use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, ErrorKind, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::SystemTime;

use serde_json::Value;

use super::{Ended, Headless, Runtime, Seen, Turn, Watch};

/// The codex version this adapter was validated against: the last one the
/// codex boundary test passed on. `cargo xtask validate` writes the file
/// after that test passed, and `make boundary` runs it.
const VALIDATED_VERSION: &str = include_str!("../validated/codex").trim_ascii();

pub struct Codex;

impl Runtime for Codex {
    fn validated_version(&self) -> &'static str {
        VALIDATED_VERSION
    }

    fn headless_args(&self, run: &Headless<'_>) -> Result<Vec<String>, String> {
        if let Turn::Fresh {
            session_id: Some(session_id),
            ..
        } = run.turn
        {
            return Err(format!(
                "codex chooses the id of a new session itself, so it cannot open the session \
{session_id:?}"
            ));
        }
        let mut args = vec!["exec".to_string()];
        if let Turn::Resume { .. } = run.turn {
            args.push("resume".to_string());
        }
        args.push("--json".to_string());
        if let Some(model) = run.model {
            args.extend(["-m".to_string(), model.to_string()]);
        }
        args.extend(run.profile.iter().cloned());
        args.push("--".to_string());
        if let Turn::Resume { session_id } = run.turn {
            args.push(session_id.to_string());
        }
        args.push(run.message.to_string());
        Ok(args)
    }

    fn interactive_args(&self, session_id: &str) -> Vec<String> {
        vec!["resume".to_string(), session_id.to_string()]
    }

    // The `--json` stream names no sandbox or approval setting, so the
    // profile has nothing to be checked against.
    fn watch(&self, run: &Headless<'_>) -> Box<dyn Watch> {
        Box::new(CodexWatch::new(SessionFiles::from_env(), &run.turn))
    }
}

/// The session files codex keeps: `$CODEX_HOME/sessions`, where
/// `CODEX_HOME` defaults to `~/.codex`.
#[derive(Debug, Clone)]
struct SessionFiles {
    root: Option<PathBuf>,
}

impl SessionFiles {
    fn from_env() -> Self {
        let home = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")));
        Self {
            root: home.map(|home| home.join("sessions")),
        }
    }

    /// The session file of `session_id`, or `None` when codex keeps none.
    ///
    /// codex names it `rollout-<time>-<id>.jsonl` in a directory per day. A
    /// session it rolled back also has files named
    /// `rollout-<time>-<id>_<rollout id>.jsonl`, and the newest one is the
    /// one it writes.
    fn find(&self, session_id: &str) -> Result<Option<PathBuf>, String> {
        let Some(root) = &self.root else {
            return Ok(None);
        };
        let mut found = Vec::new();
        collect(root, session_id, &mut found)?;
        let mut newest: Option<(SystemTime, PathBuf)> = None;
        for path in found {
            let modified = fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
                newest = Some((modified, path));
            }
        }
        Ok(newest.map(|(_, path)| path))
    }
}

/// Add every session file of `session_id` under `dir` to `found`.
fn collect(dir: &Path, session_id: &str, found: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if kind.is_dir() {
            collect(&path, session_id, found)?;
        } else if names_session(&entry.file_name().to_string_lossy(), session_id) {
            found.push(path);
        }
    }
    Ok(())
}

/// Whether `file_name` is a session file of `session_id`:
/// `rollout-YYYY-MM-DDThh-mm-ss-<id>.jsonl`, or the same with
/// `_<rollout id>` after the id.
fn names_session(file_name: &str, session_id: &str) -> bool {
    let Some(core) = file_name
        .strip_prefix("rollout-")
        .and_then(|rest| rest.strip_suffix(".jsonl"))
    else {
        return false;
    };
    let (Some("-"), Some(ids)) = (core.get(19..20), core.get(20..)) else {
        return false;
    };
    ids.split_once('_').map_or(ids, |(thread, _)| thread) == session_id
}

/// What the stream of one run said so far.
#[derive(Debug)]
struct CodexWatch {
    files: SessionFiles,
    /// For a resume: the session file and its length before the run, so the
    /// limit check reads only what this run added. The error, when the file
    /// could not be read.
    resumed_from: Result<Option<(PathBuf, u64)>, String>,
    completed: bool,
    /// The text of the last agent message.
    answer: Option<String>,
    /// The message of the last `turn.failed` or `error` event.
    failure: Option<String>,
}

impl CodexWatch {
    fn new(files: SessionFiles, turn: &Turn<'_>) -> Self {
        let resumed_from = match turn {
            Turn::Fresh { .. } => Ok(None),
            Turn::Resume { session_id } => files.find(session_id).and_then(|found| {
                found
                    .map(|path| {
                        fs::metadata(&path)
                            .map(|meta| (path.clone(), meta.len()))
                            .map_err(|e| format!("cannot read {}: {e}", path.display()))
                    })
                    .transpose()
            }),
        };
        Self {
            files,
            resumed_from,
            completed: false,
            answer: None,
            failure: None,
        }
    }

    /// The usage limit this run hit, with its reset time, or `None` when
    /// the session file shows none.
    fn limit(&self, session_id: Option<&str>) -> Result<Option<Option<i64>>, String> {
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        let Some(path) = self.files.find(session_id)? else {
            return Ok(None);
        };
        let offset = match &self.resumed_from {
            Ok(Some((before, length))) if *before == path => *length,
            Ok(_) => 0,
            Err(e) => return Err(e.clone()),
        };
        Ok(last_rate_limits(&path, offset)?
            .as_ref()
            .and_then(reached_limit))
    }
}

impl Watch for CodexWatch {
    fn line(&mut self, line: &str) -> Result<Seen, String> {
        let event: Value = serde_json::from_str(line)
            .map_err(|e| format!("codex printed a line that is not JSON ({e}): {line}"))?;
        match event["type"].as_str() {
            Some("thread.started") => {
                return match event["thread_id"].as_str() {
                    Some(id) => Ok(Seen {
                        session_id: Some(id.to_string()),
                        problem: None,
                    }),
                    None => Err(format!("codex's thread.started names no thread_id: {line}")),
                };
            }
            Some("turn.completed") => self.completed = true,
            Some("item.completed") if event["item"]["type"] == "agent_message" => {
                match event["item"]["text"].as_str() {
                    Some(text) => self.answer = Some(text.to_string()),
                    None => {
                        return Err(format!("codex's agent_message item has no text: {line}"));
                    }
                }
            }
            Some("turn.failed") => self.failure = Some(message(&event["error"], line)),
            Some("error") => self.failure = Some(message(&event, line)),
            _ => {}
        }
        Ok(Seen::default())
    }

    fn end(self: Box<Self>, exit: ExitStatus, session_id: Option<&str>) -> Ended {
        if exit.success() && self.completed {
            return Ended::Completed {
                answer: self.answer,
            };
        }
        let reason = match (&self.failure, exit.success()) {
            (Some(message), _) => format!("codex: {message}"),
            (None, false) => format!("codex exited with {exit}"),
            (None, true) => "codex ended with no turn.completed event".to_string(),
        };
        match self.limit(session_id) {
            Ok(Some(resets_at)) => Ended::Limited { resets_at },
            Ok(None) => Ended::Failed(reason),
            Err(e) => Ended::Failed(format!(
                "{reason}; the session file could not be read for a usage limit: {e}"
            )),
        }
    }
}

/// The `message` of a codex error, or the whole line when it has none.
fn message(error: &Value, line: &str) -> String {
    error["message"].as_str().unwrap_or(line).to_string()
}

/// The last `rate_limits` that a `token_count` event recorded in the session
/// file at `path`, at or after byte `offset`.
///
/// A line that is not JSON is skipped: codex may be writing the last line
/// still, and the lines of a session file are codex's, not a stream heinzel
/// reports on.
fn last_rate_limits(path: &Path, offset: u64) -> Result<Option<Value>, String> {
    let cannot = |e: io::Error| format!("cannot read {}: {e}", path.display());
    let mut file = File::open(path).map_err(cannot)?;
    file.seek(SeekFrom::Start(offset)).map_err(cannot)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut last = None;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).map_err(cannot)? == 0 {
            return Ok(last);
        }
        let Ok(mut record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if record["type"] == "event_msg"
            && record["payload"]["type"] == "token_count"
            && record["payload"]["rate_limits"].is_object()
        {
            last = Some(record["payload"]["rate_limits"].take());
        }
    }
}

/// The reset time of the limit that `rate_limits` shows reached, or `None`
/// when it shows none. A limit with no known reset is `Some(None)`.
fn reached_limit(rate_limits: &Value) -> Option<Option<i64>> {
    let full: Vec<&Value> = ["primary", "secondary"]
        .iter()
        .map(|name| &rate_limits[name])
        .filter(|window| {
            window["used_percent"]
                .as_f64()
                .is_some_and(|used| used >= 100.0)
        })
        .collect();
    let reached = !full.is_empty()
        || !rate_limits["rate_limit_reached_type"].is_null()
        || rate_limits["spend_control_reached"] == true;
    reached.then(|| {
        full.iter()
            .filter_map(|window| window["resets_at"].as_i64())
            .max()
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    fn exit(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    /// A fresh scratch directory for session files.
    fn scratch(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("heinzel-codex-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const ID: &str = "019a0c2e-7a3b-7c11-9d0e-5f6a7b8c9d0e";

    fn session_file(root: &Path) -> PathBuf {
        let day = root.join("2026/09/30");
        fs::create_dir_all(&day).unwrap();
        day.join(format!("rollout-2026-09-30T12-00-00-{ID}.jsonl"))
    }

    fn token_count(primary_used: f64, resets_at: i64) -> String {
        format!(
            r#"{{"timestamp":"2026-09-30T12:00:01Z","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"primary":{{"used_percent":{primary_used},"window_minutes":300,"resets_at":{resets_at}}},"secondary":{{"used_percent":40.0,"window_minutes":10080,"resets_at":1790500000}},"rate_limit_reached_type":null}}}}}}"#
        )
    }

    fn watch_on(root: &Path, turn: &Turn<'_>, stream: &[&str]) -> Box<CodexWatch> {
        let mut watch = Box::new(CodexWatch::new(
            SessionFiles {
                root: Some(root.to_path_buf()),
            },
            turn,
        ));
        for line in stream {
            watch.line(line).unwrap();
        }
        watch
    }

    const FAILED: &str =
        r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit."}}"#;

    const FRESH: Turn<'static> = Turn::Fresh {
        name: Some("k"),
        session_id: None,
    };

    #[test]
    fn a_resume_names_the_session_before_the_message() {
        let profile = ["-c".to_string(), "x=1".to_string()];
        let run = |turn| {
            Codex.headless_args(&Headless {
                turn,
                profile: &profile,
                model: None,
                message: "-go",
            })
        };
        assert_eq!(
            run(FRESH).unwrap(),
            ["exec", "--json", "-c", "x=1", "--", "-go"]
        );
        assert_eq!(
            run(Turn::Resume { session_id: ID }).unwrap(),
            ["exec", "resume", "--json", "-c", "x=1", "--", ID, "-go"]
        );
    }

    /// codex names a new session itself. An id the caller chose is refused,
    /// not dropped.
    #[test]
    fn a_session_id_for_a_new_session_is_refused() {
        let error = Codex
            .headless_args(&Headless {
                turn: Turn::Fresh {
                    name: None,
                    session_id: Some(ID),
                },
                profile: &[],
                model: None,
                message: "go",
            })
            .unwrap_err();
        assert!(error.contains(ID), "{error}");
    }

    #[test]
    fn thread_started_names_the_session() {
        let mut watch = CodexWatch::new(SessionFiles { root: None }, &FRESH);
        let seen = watch
            .line(r#"{"type":"thread.started","thread_id":"t-1"}"#)
            .unwrap();
        assert_eq!(seen.session_id.as_deref(), Some("t-1"));
    }

    /// The last agent message is the answer. A reasoning item is not one.
    #[test]
    fn a_completed_turn_completes_with_the_last_agent_message() {
        let root = scratch("completed");
        let watch = watch_on(
            &root,
            &FRESH,
            &[
                r#"{"type":"turn.started"}"#,
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"first"}}"#,
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"last"}}"#,
                r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#,
                r#"{"type":"turn.completed","usage":{}}"#,
            ],
        );
        assert_eq!(
            watch.end(exit(0), Some(ID)),
            Ended::Completed {
                answer: Some("last".to_string())
            }
        );
        let watch = watch_on(&root, &FRESH, &[r#"{"type":"turn.completed","usage":{}}"#]);
        assert_eq!(
            watch.end(exit(0), Some(ID)),
            Ended::Completed { answer: None }
        );
    }

    /// codex exits 0 when the agent's actions fail, so only the stream says
    /// the turn did not finish.
    #[test]
    fn an_exit_without_turn_completed_fails() {
        let root = scratch("no-completed");
        let watch = watch_on(&root, &FRESH, &[r#"{"type":"turn.started"}"#]);
        assert_eq!(
            watch.end(exit(0), Some(ID)),
            Ended::Failed("codex ended with no turn.completed event".to_string())
        );
    }

    #[test]
    fn a_failed_turn_without_a_full_window_fails_with_its_message() {
        let root = scratch("failed");
        fs::write(session_file(&root), token_count(62.0, 1_790_000_000) + "\n").unwrap();
        let watch = watch_on(&root, &FRESH, &[FAILED]);
        assert_eq!(
            watch.end(exit(1), Some(ID)),
            Ended::Failed("codex: You've hit your usage limit.".to_string())
        );
    }

    #[test]
    fn a_failed_turn_with_a_full_window_is_a_limit_with_its_reset() {
        let root = scratch("limited");
        let file = session_file(&root);
        fs::write(
            &file,
            token_count(62.0, 1) + "\n" + &token_count(100.0, 1_790_000_000) + "\n",
        )
        .unwrap();
        let watch = watch_on(&root, &FRESH, &[FAILED]);
        assert_eq!(
            watch.end(exit(1), Some(ID)),
            Ended::Limited {
                resets_at: Some(1_790_000_000)
            }
        );
    }

    /// A full window that an earlier run recorded says nothing about this
    /// run: a resume reads only what it added.
    #[test]
    fn a_resume_reads_only_its_own_records() {
        let root = scratch("resume");
        let file = session_file(&root);
        fs::write(&file, token_count(100.0, 1_790_000_000) + "\n").unwrap();
        let watch = watch_on(&root, &Turn::Resume { session_id: ID }, &[FAILED]);
        // The run adds a record, but no rate limits. Only the offset keeps
        // the earlier full window out.
        let added = r#"{"timestamp":"2026-09-30T12:00:02Z","type":"event_msg","payload":{"type":"turn_started"}}"#;
        fs::write(
            &file,
            token_count(100.0, 1_790_000_000) + "\n" + added + "\n",
        )
        .unwrap();
        assert_eq!(
            watch.end(exit(1), Some(ID)),
            Ended::Failed("codex: You've hit your usage limit.".to_string())
        );
    }

    /// A spend cap or spent credits leave the windows below full, and name
    /// the limit instead. Its reset is unknown.
    #[test]
    fn a_named_limit_is_a_limit_without_a_reset() {
        let rate_limits: Value = serde_json::from_str(
            r#"{"primary":{"used_percent":20.0},"rate_limit_reached_type":"workspace_member_credits_depleted"}"#,
        )
        .unwrap();
        assert_eq!(reached_limit(&rate_limits), Some(None));
        let rate_limits: Value =
            serde_json::from_str(r#"{"primary":{"used_percent":99.5,"resets_at":5}}"#).unwrap();
        assert_eq!(reached_limit(&rate_limits), None);
    }

    #[test]
    fn a_session_file_is_found_by_its_id() {
        assert!(names_session(
            &format!("rollout-2026-09-30T12-00-00-{ID}.jsonl"),
            ID
        ));
        assert!(names_session(
            &format!("rollout-2026-09-30T12-00-00-{ID}_0199.jsonl"),
            ID
        ));
        assert!(!names_session(
            &format!("rollout-2026-09-30T12-00-00-{ID}x.jsonl"),
            ID
        ));
        assert!(!names_session(&format!("{ID}.jsonl"), ID));
        assert!(!names_session("rollout-short.jsonl", ID));
    }
}
