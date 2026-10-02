//! The repository's own tasks.
//!
//!     cargo xtask validate [--all]
//!
//! `validate` reads the version of each agent CLI on `PATH`, and skips a CLI
//! that is not on `PATH`. For each CLI whose version the record in
//! `runtime/validated/` does not hold, it runs the boundary test of that
//! CLI. When every test it ran passed, it writes the versions they passed on
//! to the record. `--all` runs the boundary test of every installed CLI,
//! whatever the record holds.
//!
//! The command asks nothing, and it never commits or pushes. Its exit code
//! says how it ended:
//!
//! - `0`: the record holds the version of every installed CLI. stdout lists
//!   each record file that the command changed, one path per line, relative
//!   to the repository root. stdout is empty when nothing changed.
//! - `1`: a boundary test failed. The record is as it was. stderr names the
//!   CLI and the test.
//! - `2`: the command could not do its check. For example, the `--version`
//!   of a CLI fails, or the boundary tests do not build. The record is as it
//!   was, unless stderr names the record files that the command replaced
//!   before an error.
//!
//! Any other exit code comes from cargo, which could not build or run the
//! command.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use heinzel_runtime::{RuntimeName, installed_version};

/// The record: one file per runtime that holds the CLI version its boundary
/// test last passed on. Each adapter compiles its file in.
const RECORD: &str = "runtime/validated";

/// Where a boundary test writes the version it passed on.
const SCRATCH: &str = "target/boundary-record";

const USAGE: &str = "usage: cargo xtask validate [--all]";

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let all = match args.as_slice() {
        [task] if task == "validate" => false,
        [task, flag] if task == "validate" && flag == "--all" => true,
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let workspace = Workspace {
        cargo: env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        root: root.clone(),
    };
    match validate(&workspace, &root.join(RECORD), &root.join(SCRATCH), all) {
        Ok(changed) => {
            for runtime in changed {
                println!("{RECORD}/{}", runtime.as_str());
            }
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!("{failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}

/// Why a validation failed.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// The boundary test `test` failed on `version` of the CLI of `runtime`.
    /// The record is as it was.
    Failed {
        runtime: RuntimeName,
        version: String,
        test: String,
    },
    /// The validation could not do its check, for the reason given. The
    /// record is as it was.
    Broken(String),
    /// Every test passed, and the record files in `replaced` hold their new
    /// versions. Then the next file could not be replaced, for the reason
    /// given, and it holds its old version.
    Torn {
        replaced: Vec<PathBuf>,
        reason: String,
    },
}

impl Failure {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Failed { .. } => 1,
            Self::Broken(_) | Self::Torn { .. } => 2,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed {
                runtime,
                version,
                test,
            } => write!(
                f,
                "the boundary test {test} failed on {} {version}; the record is as it was",
                runtime.as_str()
            ),
            Self::Broken(reason) => write!(f, "{reason}; the record is as it was"),
            Self::Torn { replaced, reason } => {
                let replaced = replaced
                    .iter()
                    .map(|file| file.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "{reason}; every test passed, and the record files that hold their new \
version are {replaced}"
                )
            }
        }
    }
}

/// What a validation needs from outside: the CLIs on `PATH`, and the
/// boundary tests that run them.
trait Boundary {
    /// The version of the CLI of `runtime` on `PATH`, or `None` when no
    /// program of that name is on `PATH`.
    fn installed(&self, runtime: RuntimeName) -> Result<Option<String>, String>;

    /// Build the boundary tests. After a build, a run that fails is a test
    /// that failed.
    fn build(&self) -> Result<(), String>;

    /// Run the boundary test `test`, which writes the version it passed on
    /// to `scratch`. `true` when the test passed.
    fn passes(&self, test: &str, scratch: &Path) -> Result<bool, String>;
}

/// The boundary tests of this workspace, run through `cargo`.
struct Workspace {
    cargo: OsString,
    root: PathBuf,
}

impl Workspace {
    /// A `cargo test` of the boundary tests, with `args` after its own.
    /// Nothing the tests run can wait for input, and their output goes to
    /// stderr, so that stdout holds only what the command changed.
    fn cargo_test(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.cargo);
        command
            .args(["test", "--package", "heinzel", "--test", "boundary"])
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(io::stderr());
        command
    }
}

impl Boundary for Workspace {
    fn installed(&self, runtime: RuntimeName) -> Result<Option<String>, String> {
        let name = runtime.as_str();
        let on_path = env::var_os("PATH")
            .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(name).is_file()));
        if on_path {
            installed_version(Path::new(name)).map(Some)
        } else {
            Ok(None)
        }
    }

    fn build(&self) -> Result<(), String> {
        let status = self
            .cargo_test(&["--no-run"])
            .status()
            .map_err(|e| format!("cannot run cargo ({e})"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("the boundary tests do not build ({status})"))
        }
    }

    fn passes(&self, test: &str, scratch: &Path) -> Result<bool, String> {
        let status = self
            .cargo_test(&["--", "--ignored", "--exact", test])
            .env("BOUNDARY_RECORD", scratch)
            .status()
            .map_err(|e| format!("cannot run cargo ({e})"))?;
        match status.code() {
            Some(0) => Ok(true),
            Some(_) => Ok(false),
            None => Err(format!("the boundary test {test} ended ({status})")),
        }
    }
}

/// Run the boundary test of each installed runtime whose version `record`
/// does not hold, or of every installed runtime when `all` is set. When
/// every test passed, write the versions they passed on to `record`.
/// Returns the runtimes whose record changed.
///
/// A test that fails stops the validation, and the tests after it do not
/// run: each run uses the account's usage.
fn validate(
    boundary: &impl Boundary,
    record: &Path,
    scratch: &Path,
    all: bool,
) -> Result<Vec<RuntimeName>, Failure> {
    let mut due = Vec::new();
    for runtime in RuntimeName::ALL {
        let name = runtime.as_str();
        let Some(installed) = boundary.installed(runtime).map_err(Failure::Broken)? else {
            eprintln!("{name} is not on PATH");
            continue;
        };
        let held = read_version(&record.join(name))?;
        if installed == held && !all {
            eprintln!("{name} {installed} is validated");
        } else {
            due.push((runtime, held, installed));
        }
    }
    if due.is_empty() {
        return Ok(Vec::new());
    }

    boundary.build().map_err(Failure::Broken)?;
    clear(scratch)?;
    for (runtime, _, installed) in &due {
        let test = test_name(*runtime);
        eprintln!("{} {installed}: running {test}", runtime.as_str());
        if !boundary.passes(&test, scratch).map_err(Failure::Broken)? {
            return Err(Failure::Failed {
                runtime: *runtime,
                version: installed.clone(),
                test,
            });
        }
    }

    // Read every version the tests passed on before the first write, so a
    // test that wrote none leaves the whole record as it was.
    let mut changed = Vec::new();
    for (runtime, held, _) in &due {
        let passed = read_version(&scratch.join(runtime.as_str())).map_err(|e| {
            Failure::Broken(format!(
                "{} passed and wrote no version: {e}",
                test_name(*runtime)
            ))
        })?;
        if passed != *held {
            changed.push((*runtime, passed));
        }
    }
    write_record(record, &changed)?;
    Ok(changed.into_iter().map(|(runtime, _)| runtime).collect())
}

/// The boundary test of `runtime`, as `tests/boundary.rs` names it. A name
/// that finds no test runs nothing. Then no version gets written, and the
/// validation fails.
fn test_name(runtime: RuntimeName) -> String {
    format!("the_{}_adapter_holds", runtime.as_str())
}

/// The version that the file at `path` holds.
fn read_version(path: &Path) -> Result<String, Failure> {
    let text = fs::read_to_string(path)
        .map_err(|e| Failure::Broken(format!("cannot read {} ({e})", path.display())))?;
    let version = text.trim();
    if version.is_empty() {
        return Err(Failure::Broken(format!("{} is empty", path.display())));
    }
    Ok(version.to_string())
}

/// Make `dir` an empty directory.
fn clear(dir: &Path) -> Result<(), Failure> {
    let cannot = |e: io::Error| Failure::Broken(format!("cannot clear {} ({e})", dir.display()));
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(cannot(e)),
        _ => {}
    }
    fs::create_dir_all(dir).map_err(cannot)
}

/// Write each version in `changed` to the file of its runtime in `record`.
/// Every new file is complete before the first one replaces an old one. On
/// an error, no new file stays beside the record.
fn write_record(record: &Path, changed: &[(RuntimeName, String)]) -> Result<(), Failure> {
    let mut staged = Vec::new();
    for (runtime, version) in changed {
        let file = record.join(runtime.as_str());
        let new = record.join(format!("{}.new", runtime.as_str()));
        let error = fs::write(&new, format!("{version}\n"))
            .err()
            .map(|e| format!("cannot write {} ({e})", new.display()));
        // A write that failed can leave a part of the file.
        staged.push((new, file));
        if let Some(reason) = error {
            remove_new(&staged);
            return Err(Failure::Broken(reason));
        }
    }

    let mut replaced = Vec::new();
    for (index, (new, file)) in staged.iter().enumerate() {
        if let Err(e) = fs::rename(new, file) {
            remove_new(&staged[index..]);
            let reason = format!("cannot replace {} ({e})", file.display());
            return Err(if replaced.is_empty() {
                Failure::Broken(reason)
            } else {
                Failure::Torn { replaced, reason }
            });
        }
        replaced.push(file.clone());
    }
    Ok(())
}

/// Remove the new file of each pair in `staged`. The error that stopped the
/// write is the one the command reports, so a file that cannot be removed
/// adds none.
fn remove_new(staged: &[(PathBuf, PathBuf)]) {
    for (new, _) in staged {
        let _ = fs::remove_file(new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const CLAUDE: &str = "2.1.286";
    const CODEX: &str = "0.159.0";

    /// A record and a scratch directory, deleted on drop.
    struct Dirs {
        root: PathBuf,
    }

    impl Dirs {
        fn new(tag: &str) -> Self {
            let root = env::temp_dir().join(format!("xtask-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("record")).unwrap();
            fs::write(root.join("record/claude"), format!("{CLAUDE}\n")).unwrap();
            fs::write(root.join("record/codex"), format!("{CODEX}\n")).unwrap();
            Self { root }
        }

        fn record(&self) -> PathBuf {
            self.root.join("record")
        }

        fn scratch(&self) -> PathBuf {
            self.root.join("scratch")
        }

        /// The files of the record, by name, with what each holds.
        fn record_files(&self) -> Vec<(String, String)> {
            let mut files: Vec<_> = fs::read_dir(self.record())
                .unwrap()
                .map(|entry| {
                    let path = entry.unwrap().path();
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    (name, fs::read_to_string(&path).unwrap())
                })
                .collect();
            files.sort();
            files
        }

        fn validate(&self, boundary: &Fake, all: bool) -> Result<Vec<RuntimeName>, Failure> {
            validate(boundary, &self.record(), &self.scratch(), all)
        }
    }

    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn the_old_record() -> Vec<(String, String)> {
        vec![
            ("claude".to_string(), format!("{CLAUDE}\n")),
            ("codex".to_string(), format!("{CODEX}\n")),
        ]
    }

    /// CLIs on `PATH` and boundary tests that do what the test says.
    struct Fake {
        claude: Result<Option<String>, String>,
        codex: Result<Option<String>, String>,
        /// The test that fails, if any.
        failing: Option<RuntimeName>,
        /// The test that passes and writes no version, if any.
        silent: Option<RuntimeName>,
        built: RefCell<bool>,
        ran: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(claude: &str, codex: &str) -> Self {
            Self {
                claude: Ok(Some(claude.to_string())),
                codex: Ok(Some(codex.to_string())),
                failing: None,
                silent: None,
                built: RefCell::new(false),
                ran: RefCell::new(Vec::new()),
            }
        }

        fn ran(&self) -> Vec<String> {
            self.ran.borrow().clone()
        }
    }

    impl Boundary for Fake {
        fn installed(&self, runtime: RuntimeName) -> Result<Option<String>, String> {
            match runtime {
                RuntimeName::Claude => self.claude.clone(),
                RuntimeName::Codex => self.codex.clone(),
            }
        }

        fn build(&self) -> Result<(), String> {
            *self.built.borrow_mut() = true;
            Ok(())
        }

        fn passes(&self, test: &str, scratch: &Path) -> Result<bool, String> {
            assert!(*self.built.borrow(), "{test} ran before the build");
            self.ran.borrow_mut().push(test.to_string());
            let runtime = RuntimeName::ALL
                .into_iter()
                .find(|runtime| test_name(*runtime) == test)
                .unwrap();
            if self.failing == Some(runtime) {
                return Ok(false);
            }
            if self.silent != Some(runtime) {
                let version = self.installed(runtime)?.unwrap();
                fs::write(scratch.join(runtime.as_str()), format!("{version}\n")).unwrap();
            }
            Ok(true)
        }
    }

    #[test]
    fn nothing_runs_when_the_record_holds_every_installed_version() {
        let dirs = Dirs::new("nothing-new");
        let fake = Fake::new(CLAUDE, CODEX);
        assert_eq!(dirs.validate(&fake, false), Ok(Vec::new()));
        assert!(!*fake.built.borrow());
        assert_eq!(fake.ran(), Vec::<String>::new());
        assert_eq!(dirs.record_files(), the_old_record());
    }

    #[test]
    fn a_new_version_runs_only_its_own_test_and_is_recorded() {
        let dirs = Dirs::new("new-claude");
        let fake = Fake::new("2.1.287", CODEX);
        assert_eq!(dirs.validate(&fake, false), Ok(vec![RuntimeName::Claude]));
        assert_eq!(fake.ran(), vec!["the_claude_adapter_holds"]);
        assert_eq!(
            dirs.record_files(),
            vec![
                ("claude".to_string(), "2.1.287\n".to_string()),
                ("codex".to_string(), format!("{CODEX}\n")),
            ]
        );
    }

    #[test]
    fn every_new_version_is_recorded_when_every_test_passed() {
        let dirs = Dirs::new("both-new");
        let fake = Fake::new("2.1.287", "0.160.0");
        assert_eq!(
            dirs.validate(&fake, false),
            Ok(vec![RuntimeName::Claude, RuntimeName::Codex])
        );
        assert_eq!(
            dirs.record_files(),
            vec![
                ("claude".to_string(), "2.1.287\n".to_string()),
                ("codex".to_string(), "0.160.0\n".to_string()),
            ]
        );
    }

    #[test]
    fn a_failed_test_stops_the_run_and_names_the_cli_and_the_test() {
        let dirs = Dirs::new("claude-fails");
        let mut fake = Fake::new("2.1.287", "0.160.0");
        fake.failing = Some(RuntimeName::Claude);
        let failure = dirs.validate(&fake, false).unwrap_err();
        assert_eq!(
            failure,
            Failure::Failed {
                runtime: RuntimeName::Claude,
                version: "2.1.287".to_string(),
                test: "the_claude_adapter_holds".to_string(),
            }
        );
        assert_eq!(failure.exit_code(), 1);
        assert_eq!(
            failure.to_string(),
            "the boundary test the_claude_adapter_holds failed on claude 2.1.287; \
the record is as it was"
        );
        assert_eq!(fake.ran(), vec!["the_claude_adapter_holds"]);
        assert_eq!(dirs.record_files(), the_old_record());
    }

    #[test]
    fn a_failed_test_leaves_the_versions_of_earlier_passes_out_of_the_record() {
        let dirs = Dirs::new("codex-fails");
        let mut fake = Fake::new("2.1.287", "0.160.0");
        fake.failing = Some(RuntimeName::Codex);
        let failure = dirs.validate(&fake, false).unwrap_err();
        assert!(
            matches!(
                failure,
                Failure::Failed {
                    runtime: RuntimeName::Codex,
                    ..
                }
            ),
            "{failure:?}"
        );
        assert_eq!(
            fake.ran(),
            vec!["the_claude_adapter_holds", "the_codex_adapter_holds"]
        );
        assert_eq!(dirs.record_files(), the_old_record());
    }

    #[test]
    fn a_cli_that_is_not_installed_is_skipped() {
        let dirs = Dirs::new("no-codex");
        let mut fake = Fake::new("2.1.287", CODEX);
        fake.codex = Ok(None);
        assert_eq!(dirs.validate(&fake, true), Ok(vec![RuntimeName::Claude]));
        assert_eq!(fake.ran(), vec!["the_claude_adapter_holds"]);
        assert_eq!(
            dirs.record_files(),
            vec![
                ("claude".to_string(), "2.1.287\n".to_string()),
                ("codex".to_string(), format!("{CODEX}\n")),
            ]
        );
    }

    #[test]
    fn a_version_that_cannot_be_read_runs_nothing() {
        let dirs = Dirs::new("broken-codex");
        let mut fake = Fake::new("2.1.287", CODEX);
        fake.codex = Err("`codex --version` failed (exit status: 4)".to_string());
        let failure = dirs.validate(&fake, false).unwrap_err();
        assert_eq!(
            failure,
            Failure::Broken("`codex --version` failed (exit status: 4)".to_string())
        );
        assert_eq!(failure.exit_code(), 2);
        assert!(!*fake.built.borrow());
        assert_eq!(dirs.record_files(), the_old_record());
    }

    #[test]
    fn a_test_that_writes_no_version_leaves_the_record() {
        let dirs = Dirs::new("silent-codex");
        let mut fake = Fake::new("2.1.287", "0.160.0");
        fake.silent = Some(RuntimeName::Codex);
        let failure = dirs.validate(&fake, false).unwrap_err();
        assert!(
            matches!(&failure, Failure::Broken(reason)
                if reason.starts_with("the_codex_adapter_holds passed and wrote no version")),
            "{failure:?}"
        );
        assert_eq!(dirs.record_files(), the_old_record());
    }

    #[test]
    fn all_runs_every_test_and_changes_nothing_on_the_same_versions() {
        let dirs = Dirs::new("all");
        let fake = Fake::new(CLAUDE, CODEX);
        assert_eq!(dirs.validate(&fake, true), Ok(Vec::new()));
        assert_eq!(
            fake.ran(),
            vec!["the_claude_adapter_holds", "the_codex_adapter_holds"]
        );
        assert_eq!(dirs.record_files(), the_old_record());
    }

    #[test]
    fn the_scratch_holds_only_what_this_run_wrote() {
        let dirs = Dirs::new("stale-scratch");
        fs::create_dir_all(dirs.scratch()).unwrap();
        fs::write(dirs.scratch().join("codex"), "0.160.0\n").unwrap();
        let mut fake = Fake::new(CLAUDE, "0.160.0");
        fake.silent = Some(RuntimeName::Codex);
        assert!(matches!(
            dirs.validate(&fake, false),
            Err(Failure::Broken(_))
        ));
        assert_eq!(dirs.record_files(), the_old_record());
    }

    #[test]
    fn a_write_that_fails_leaves_no_new_file() {
        let dirs = Dirs::new("write-fails");
        // A directory in the place of the new codex file fails its write.
        fs::create_dir(dirs.record().join("codex.new")).unwrap();
        let failure = write_record(
            &dirs.record(),
            &[
                (RuntimeName::Claude, "2.1.287".to_string()),
                (RuntimeName::Codex, "0.160.0".to_string()),
            ],
        )
        .unwrap_err();
        assert!(
            matches!(&failure, Failure::Broken(reason) if reason.starts_with("cannot write ")),
            "{failure:?}"
        );
        assert!(!dirs.record().join("claude.new").exists());
        let record = dirs.record();
        for (name, version) in the_old_record() {
            assert_eq!(fs::read_to_string(record.join(name)).unwrap(), version);
        }
    }

    #[test]
    fn a_replace_that_fails_after_another_names_the_replaced_files() {
        let dirs = Dirs::new("replace-fails");
        // A directory that holds a file cannot be replaced by a file.
        let codex = dirs.record().join("codex");
        fs::remove_file(&codex).unwrap();
        fs::create_dir_all(codex.join("inside")).unwrap();
        let failure = write_record(
            &dirs.record(),
            &[
                (RuntimeName::Claude, "2.1.287".to_string()),
                (RuntimeName::Codex, "0.160.0".to_string()),
            ],
        )
        .unwrap_err();
        let claude = dirs.record().join("claude");
        assert!(
            matches!(&failure, Failure::Torn { replaced, .. } if *replaced == vec![claude.clone()]),
            "{failure:?}"
        );
        assert_eq!(failure.exit_code(), 2);
        assert!(
            failure.to_string().ends_with(&format!(
                "every test passed, and the record files that hold their new version are {}",
                claude.display()
            )),
            "{failure}"
        );
        assert_eq!(fs::read_to_string(&claude).unwrap(), "2.1.287\n");
        assert!(!dirs.record().join("codex.new").exists());
    }
}
