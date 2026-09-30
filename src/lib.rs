//! heinzel runs one agent turn for a caller that waits for it.
//!
//! A [`Turn`] names the runtime, the program, the directory, the session, the
//! model, the arguments that set the turn's permissions, the time limit and
//! the message. [`run`] runs it to its end and returns a [`Report`]: the turn
//! finished with the session id and the agent's answer, it hit a usage limit,
//! or it failed and says why.
//!
//! A turn writes nothing to heinzel's session store. The heinzel binary keeps
//! that store for its long headless runs, and a turn is not one of them.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! let report = heinzel::run(&heinzel::Turn {
//!     runtime: heinzel::Runtime::Claude,
//!     program: "claude".into(),
//!     cwd: "/tmp".into(),
//!     session: heinzel::Session::New { id: None },
//!     model: Some("sonnet".to_string()),
//!     args: vec!["--permission-mode".to_string(), "dontAsk".to_string()],
//!     time_limit: Duration::from_secs(600),
//!     message: "Reply with the word ok.".to_string(),
//! })?;
//! if let heinzel::Outcome::Finished { session_id, answer } = report.outcome {
//!     println!("{session_id}: {answer:?}");
//! }
//! # Ok::<(), String>(())
//! ```

mod group;
mod turn;

pub use group::TERM_GRACE;
pub use turn::{Outcome, Report, Runtime, Session, Turn, run, version_note};
