//! The claude adapter.
//!
//! A headless run is `claude -p` with `--output-format stream-json`. The
//! stream opens with a `system` `init` event that names the session. A
//! `result` event ends each turn, but not the process: a session that waits
//! on a background task sends a `result` and lives on. So the run ends when
//! the process exits, and the last `result` says how its last turn ended.
//! Its `result` text is the agent's answer.
//!
//! A new session takes its name from `--name`, and its id from
//! `--session-id` when the caller chose one.
//!
//! The init event also names the `permissionMode` that claude applied. That
//! need not be the one the profile asked for: claude 2.1.285 runs `auto` as
//! `default` on some models. A run whose applied mode differs says so.
//!
//! A usage limit arrives as a `rate_limit_event` whose `rate_limit_info`
//! has the `status` `"rejected"` and, when claude knows it, a `resetsAt` in
//! seconds since the Unix epoch.

use std::process::ExitStatus;

use serde_json::Value;

use super::{Ended, Headless, Runtime, Seen, Turn, Watch};

/// The flag that names claude's permission mode.
const MODE_FLAG: &str = "--permission-mode";
/// The flag that skips every permission check. claude reports it as the
/// mode [`SKIP_MODE`].
const SKIP_FLAG: &str = "--dangerously-skip-permissions";
const SKIP_MODE: &str = "bypassPermissions";

/// The claude version this adapter was validated against.
pub const VALIDATED_VERSION: &str = "2.1.285";

pub struct Claude;

impl Runtime for Claude {
    fn validated_version(&self) -> &'static str {
        VALIDATED_VERSION
    }

    fn headless_args(&self, run: &Headless<'_>) -> Result<Vec<String>, String> {
        let mut args: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose"]
            .map(String::from)
            .into();
        match run.turn {
            Turn::Fresh { name, session_id } => {
                if let Some(name) = name {
                    args.extend(["--name".to_string(), name.to_string()]);
                }
                if let Some(session_id) = session_id {
                    args.extend(["--session-id".to_string(), session_id.to_string()]);
                }
            }
            Turn::Resume { session_id } => {
                args.extend(["--resume".to_string(), session_id.to_string()])
            }
        }
        if let Some(model) = run.model {
            args.extend(["--model".to_string(), model.to_string()]);
        }
        args.extend(run.profile.iter().cloned());
        args.extend(["--".to_string(), run.message.to_string()]);
        Ok(args)
    }

    fn interactive_args(&self, session_id: &str) -> Vec<String> {
        vec!["--resume".to_string(), session_id.to_string()]
    }

    fn watch(&self, run: &Headless<'_>) -> Box<dyn Watch> {
        Box::new(ClaudeWatch {
            asked_mode: asked_mode(run.profile),
            ..ClaudeWatch::default()
        })
    }
}

/// The permission mode that the profile arguments `profile` ask claude
/// for, or `None` when they name none. The last flag counts, as it does for
/// claude.
fn asked_mode(profile: &[String]) -> Option<String> {
    let mut asked = None;
    let mut args = profile.iter();
    while let Some(arg) = args.next() {
        if arg == MODE_FLAG {
            asked = args.next().cloned();
        } else if let Some(mode) = arg
            .strip_prefix(MODE_FLAG)
            .and_then(|rest| rest.strip_prefix('='))
        {
            asked = Some(mode.to_string());
        } else if arg == SKIP_FLAG {
            asked = Some(SKIP_MODE.to_string());
        }
    }
    asked
}

/// What the stream of one run said so far.
#[derive(Debug, Default)]
struct ClaudeWatch {
    /// The permission mode the profile asked for.
    asked_mode: Option<String>,
    /// The last usage-limit verdict: `Some` while the latest
    /// `rate_limit_event` rejected, with its reset time.
    rejected: Option<Option<i64>>,
    /// How the last turn ended: `Ok` with its answer, or `Err` with the
    /// reason when it failed.
    last_result: Option<Result<Option<String>, String>>,
}

impl Watch for ClaudeWatch {
    fn line(&mut self, line: &str) -> Result<Seen, String> {
        let event: Value = serde_json::from_str(line)
            .map_err(|e| format!("claude printed a line that is not JSON ({e}): {line}"))?;
        match event["type"].as_str() {
            Some("system") if event["subtype"] == "init" => {
                let Some(id) = event["session_id"].as_str() else {
                    return Err(format!("claude's init event names no session_id: {line}"));
                };
                return Ok(Seen {
                    session_id: Some(id.to_string()),
                    problem: self.mode_problem(event["permissionMode"].as_str()),
                });
            }
            Some("rate_limit_event") => {
                let info = &event["rate_limit_info"];
                match info["status"].as_str() {
                    Some("rejected") => self.rejected = Some(info["resetsAt"].as_i64()),
                    Some(_) => self.rejected = None,
                    None => {
                        return Err(format!(
                            "claude's rate_limit_event has no rate_limit_info.status: {line}"
                        ));
                    }
                }
            }
            Some("result") => self.last_result = Some(turn_outcome(&event)),
            _ => {}
        }
        Ok(Seen::default())
    }

    // The stream alone says how a claude run ended: no file of the session
    // is read.
    fn end(self: Box<Self>, exit: ExitStatus, _session_id: Option<&str>) -> Ended {
        let succeeded = exit.success() && matches!(self.last_result, Some(Ok(_)));
        if !succeeded && let Some(resets_at) = self.rejected {
            return Ended::Limited { resets_at };
        }
        if !exit.success() {
            let last = match &self.last_result {
                Some(Err(reason)) => format!("; its last turn: {reason}"),
                _ => String::new(),
            };
            return Ended::Failed(format!("claude exited with {exit}{last}"));
        }
        match self.last_result {
            Some(Ok(answer)) => Ended::Completed { answer },
            Some(Err(reason)) => Ended::Failed(reason),
            None => Ended::Failed("claude exited with no result event".to_string()),
        }
    }
}

impl ClaudeWatch {
    /// What the caller must hear about the `applied` mode that the init
    /// event names, or `None` when it is the mode the profile asked for.
    fn mode_problem(&self, applied: Option<&str>) -> Option<String> {
        let asked = self.asked_mode.as_deref()?;
        match applied {
            Some(applied) if applied == asked => None,
            Some(applied) => Some(format!(
                "the profile asked claude for the permission mode {asked:?}, and claude applied \
{applied:?}"
            )),
            None => Some(format!(
                "the profile asked claude for the permission mode {asked:?}, and claude's init \
event names no permissionMode"
            )),
        }
    }
}

/// How the turn that a `result` event closes ended: its answer, the
/// `result` text, when it succeeded.
fn turn_outcome(result: &Value) -> Result<Option<String>, String> {
    let subtype = result["subtype"].as_str().unwrap_or("no subtype");
    let is_error = result["is_error"].as_bool().unwrap_or(false);
    if subtype == "success" && !is_error {
        return Ok(result["result"].as_str().map(str::to_string));
    }
    let detail = result["result"]
        .as_str()
        .map(|text| format!(": {text}"))
        .unwrap_or_default();
    Err(format!("claude ended its turn with {subtype}{detail}"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    fn exit(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    /// The lines of `stream`, read by a fresh watch, and the session id the
    /// watch found.
    fn watched(stream: &[&str]) -> (Box<ClaudeWatch>, Option<String>) {
        let mut watch = Box::new(ClaudeWatch::default());
        let mut session = None;
        for line in stream {
            if let Some(id) = watch.line(line).unwrap().session_id {
                session = Some(id);
            }
        }
        (watch, session)
    }

    /// What a watch for the profile `profile` sees in `line`.
    fn seen_under(profile: &[&str], line: &str) -> Seen {
        let profile: Vec<String> = profile.iter().map(|arg| arg.to_string()).collect();
        let mut watch = Claude.watch(&Headless {
            turn: Turn::Fresh {
                name: Some("k"),
                session_id: None,
            },
            profile: &profile,
            model: None,
            message: "go",
        });
        watch.line(line).unwrap()
    }

    fn init_in(mode: &str) -> String {
        format!(
            r#"{{"type":"system","subtype":"init","session_id":"5b1e","permissionMode":"{mode}"}}"#
        )
    }

    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"5b1e"}"#;
    const SUCCESS: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#;

    #[test]
    fn a_fresh_run_names_the_key_and_a_resume_names_the_session() {
        let profile = ["--permission-mode".to_string(), "auto".to_string()];
        let run = |turn| {
            Claude
                .headless_args(&Headless {
                    turn,
                    profile: &profile,
                    model: Some("opus"),
                    message: "-go",
                })
                .unwrap()
        };
        assert_eq!(
            run(Turn::Fresh {
                name: Some("k"),
                session_id: None
            }),
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--name",
                "k",
                "--model",
                "opus",
                "--permission-mode",
                "auto",
                "--",
                "-go"
            ]
        );
        assert_eq!(
            run(Turn::Resume { session_id: "5b1e" })[4..6],
            ["--resume", "5b1e"]
        );
        assert_eq!(
            run(Turn::Fresh {
                name: None,
                session_id: Some("5b1e")
            })[4..8],
            ["--session-id", "5b1e", "--model", "opus"]
        );
    }

    #[test]
    fn the_init_event_names_the_session() {
        let (_, session) = watched(&[INIT]);
        assert_eq!(session.as_deref(), Some("5b1e"));
    }

    /// claude 2.1.285 runs `auto` as `default` on some models. The run says
    /// so, and it still names its session.
    #[test]
    fn a_mode_claude_overrode_is_a_problem() {
        let seen = seen_under(&["--permission-mode", "auto"], &init_in("default"));
        assert_eq!(
            seen,
            Seen {
                session_id: Some("5b1e".to_string()),
                problem: Some(
                    "the profile asked claude for the permission mode \"auto\", and claude \
applied \"default\""
                        .to_string()
                ),
            }
        );
    }

    #[test]
    fn the_mode_the_profile_asked_for_is_no_problem() {
        let asked = seen_under(&["--permission-mode", "auto"], &init_in("auto"));
        assert_eq!(asked.problem, None);
        let inline = seen_under(&["--permission-mode=plan"], &init_in("plan"));
        assert_eq!(inline.problem, None);
        let skipped = seen_under(
            &["--dangerously-skip-permissions"],
            &init_in("bypassPermissions"),
        );
        assert_eq!(skipped.problem, None);
        let last = seen_under(
            &["--permission-mode", "plan", "--permission-mode", "auto"],
            &init_in("auto"),
        );
        assert_eq!(last.problem, None);
    }

    /// A profile that names no mode leaves the choice to claude.
    #[test]
    fn a_profile_without_a_mode_checks_none() {
        assert_eq!(seen_under(&[], &init_in("default")).problem, None);
    }

    /// A mode the profile asked for that claude does not confirm is not
    /// silently taken as applied.
    #[test]
    fn an_init_without_a_mode_is_a_problem() {
        let seen = seen_under(&["--permission-mode", "auto"], INIT);
        assert!(
            seen.problem
                .is_some_and(|problem| problem.contains("names no permissionMode")),
        );
    }

    /// A run that completed with the answer `answer`.
    fn answered(answer: &str) -> Ended {
        Ended::Completed {
            answer: Some(answer.to_string()),
        }
    }

    #[test]
    fn a_turn_that_succeeded_completes_with_its_answer() {
        let (watch, _) = watched(&[INIT, SUCCESS]);
        assert_eq!(watch.end(exit(0), None), answered("ok"));
    }

    /// Each wait on a background task ends in a result of its own, and the
    /// last one carries the answer the run ended with.
    #[test]
    fn the_answer_is_the_last_results() {
        let later = r#"{"type":"result","subtype":"success","is_error":false,"result":"later"}"#;
        let (watch, _) = watched(&[INIT, SUCCESS, later]);
        assert_eq!(watch.end(exit(0), None), answered("later"));
        let silent = r#"{"type":"result","subtype":"success","is_error":false}"#;
        let (watch, _) = watched(&[INIT, SUCCESS, silent]);
        assert_eq!(watch.end(exit(0), None), Ended::Completed { answer: None });
    }

    /// A session that waited on a background task sends a `result` for each
    /// wait. The last one says how the run ended.
    #[test]
    fn the_last_result_decides() {
        let failed = r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#;
        let (watch, _) = watched(&[INIT, SUCCESS, failed]);
        assert_eq!(
            watch.end(exit(0), None),
            Ended::Failed("claude ended its turn with error_during_execution".to_string())
        );
        let (watch, _) = watched(&[INIT, failed, SUCCESS]);
        assert_eq!(watch.end(exit(0), None), answered("ok"));
    }

    /// `is_error` fails a turn even under the subtype `success`.
    #[test]
    fn an_error_result_fails() {
        let error = r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error"}"#;
        let (watch, _) = watched(&[INIT, error]);
        assert_eq!(
            watch.end(exit(0), None),
            Ended::Failed("claude ended its turn with success: API Error".to_string())
        );
    }

    #[test]
    fn a_nonzero_exit_fails_after_a_success() {
        let (watch, _) = watched(&[INIT, SUCCESS]);
        assert_eq!(
            watch.end(exit(1), None),
            Ended::Failed(format!("claude exited with {}", exit(1)))
        );
    }

    #[test]
    fn an_exit_without_a_result_fails() {
        let (watch, _) = watched(&[INIT]);
        assert_eq!(
            watch.end(exit(0), None),
            Ended::Failed("claude exited with no result event".to_string())
        );
    }

    #[test]
    fn a_rejected_rate_limit_is_a_limit_with_its_reset() {
        let rejected = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790000000,"rateLimitType":"five_hour"}}"#;
        let error = r#"{"type":"result","subtype":"success","is_error":true,"result":"limit"}"#;
        let (watch, _) = watched(&[INIT, rejected, error]);
        assert_eq!(
            watch.end(exit(1), None),
            Ended::Limited {
                resets_at: Some(1_790_000_000)
            }
        );
    }

    /// A limit that a later event lifted, or a turn that succeeded after
    /// it, is no limit.
    #[test]
    fn a_lifted_limit_is_no_limit() {
        let rejected = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#;
        let allowed = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#;
        let (watch, _) = watched(&[INIT, rejected, allowed]);
        assert_eq!(
            watch.end(exit(1), None),
            Ended::Failed(format!("claude exited with {}", exit(1)))
        );
        let (watch, _) = watched(&[INIT, rejected, SUCCESS]);
        assert_eq!(watch.end(exit(0), None), answered("ok"));
    }

    #[test]
    fn a_line_that_is_not_json_is_an_error() {
        let mut watch = ClaudeWatch::default();
        let error = watch.line("Loading...").unwrap_err();
        assert!(error.contains("Loading..."), "{error}");
    }
}
