//! The command line.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use heinzel_runtime::RuntimeName;

use crate::config::DEFAULT_PROFILE;
use crate::holder::{self, HOLD_COMMAND};
use crate::session::{self, Report, StartSpec};
use crate::store::{Home, Key};

const USAGE: &str = "\
usage:
  heinzel start <key> --runtime <claude|codex> --done-file <path> [--cwd <dir>]
                [--profile <name>] [--model <model>] (--message <text> | --message-file <path>)
  heinzel continue <key> (--message <text> | --message-file <path>)
  heinzel open <key>
  heinzel status <key>
  heinzel wait <key>
  heinzel stop <key>

Each command prints the session as one JSON object. An open prints it on the
last line, after the agent's own output. A message file `-` is read from
stdin. The profile defaults to `auto`.";

/// Run the command line `args`, without the program name.
pub fn run(args: Vec<OsString>) -> ExitCode {
    match dispatch(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "heinzel: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: Vec<OsString>) -> Result<(), String> {
    let args = args
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|arg| format!("an argument is not UTF-8: {arg:?}"))
        })
        .collect::<Result<Vec<String>, String>>()?;
    let Some((command, rest)) = args.split_first() else {
        return Err(USAGE.to_string());
    };
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        return print(USAGE);
    }
    let home = Home::from_env()?;
    match command.as_str() {
        "start" => {
            let mut parsed = Parsed::new(
                rest,
                &[
                    "--runtime",
                    "--done-file",
                    "--cwd",
                    "--profile",
                    "--model",
                    "--message",
                    "--message-file",
                ],
            )?;
            let here = env::current_dir()
                .map_err(|e| format!("cannot read the current directory: {e}"))?;
            let spec = StartSpec {
                runtime: RuntimeName::parse(&parsed.required("--runtime")?)?,
                done_file: here.join(parsed.required("--done-file")?),
                cwd: parsed
                    .take("--cwd")
                    .map_or_else(|| here.clone(), |cwd| here.join(cwd)),
                profile: parsed
                    .take("--profile")
                    .unwrap_or_else(|| DEFAULT_PROFILE.to_string()),
                model: parsed.take("--model"),
            };
            let message = parsed.message()?;
            let started = session::start(&home, &parsed.key, spec, &message)?;
            if let Some(note) = started.version_note {
                let _ = writeln!(io::stderr(), "heinzel: warning: {note}");
            }
            print_report(&started.report)
        }
        "continue" => {
            let parsed = Parsed::new(rest, &["--message", "--message-file"])?;
            let message = parsed.message()?;
            print_report(&session::resume(&home, &parsed.key, &message)?)
        }
        "open" => {
            let parsed = Parsed::new(rest, &[])?;
            print_report(&session::open(&home, &parsed.key)?)
        }
        "status" => print_report(&session::status(&home, &Parsed::new(rest, &[])?.key)?),
        "wait" => print_report(&session::wait(&home, &Parsed::new(rest, &[])?.key)?),
        "stop" => print_report(&session::stop(&home, &Parsed::new(rest, &[])?.key)?),
        HOLD_COMMAND => {
            let [key] = rest else {
                return Err(format!("usage: heinzel {HOLD_COMMAND} <key>"));
            };
            holder::hold(&home, &Key::parse(key)?)
        }
        other => Err(format!("no command is named {other:?}\n{USAGE}")),
    }
}

/// A key and the options after it.
struct Parsed {
    key: Key,
    options: BTreeMap<String, String>,
}

impl Parsed {
    /// Parse `args` as a key, then `--name value` pairs out of `allowed`.
    fn new(args: &[String], allowed: &[&str]) -> Result<Self, String> {
        let Some((key, mut rest)) = args.split_first() else {
            return Err(format!("a key is missing\n{USAGE}"));
        };
        let key = Key::parse(key)?;
        let mut options = BTreeMap::new();
        while let Some((name, after)) = rest.split_first() {
            if !allowed.contains(&name.as_str()) {
                return Err(format!("unknown argument {name:?}\n{USAGE}"));
            }
            let Some((value, after)) = after.split_first() else {
                return Err(format!("{name} needs a value"));
            };
            if options.insert(name.clone(), value.clone()).is_some() {
                return Err(format!("{name} is given twice"));
            }
            rest = after;
        }
        Ok(Self { key, options })
    }

    fn take(&mut self, name: &str) -> Option<String> {
        self.options.remove(name)
    }

    fn required(&mut self, name: &str) -> Result<String, String> {
        self.take(name).ok_or_else(|| format!("{name} is missing"))
    }

    /// The message: `--message`, or the contents of `--message-file`.
    fn message(&self) -> Result<String, String> {
        match (
            self.options.get("--message"),
            self.options.get("--message-file"),
        ) {
            (Some(text), None) => Ok(text.clone()),
            (None, Some(path)) if path == "-" => {
                let mut text = String::new();
                io::stdin()
                    .read_to_string(&mut text)
                    .map_err(|e| format!("cannot read the message from stdin: {e}"))?;
                Ok(text)
            }
            (None, Some(path)) => fs::read_to_string(PathBuf::from(path))
                .map_err(|e| format!("cannot read the message file {path}: {e}")),
            _ => Err("give the message by exactly one of --message and --message-file".to_string()),
        }
    }
}

fn print_report(report: &Report) -> Result<(), String> {
    let json =
        serde_json::to_string(report).map_err(|e| format!("cannot encode the report: {e}"))?;
    print(&json)
}

fn print(text: &str) -> Result<(), String> {
    writeln!(io::stdout(), "{text}").map_err(|e| format!("cannot write to stdout: {e}"))
}
