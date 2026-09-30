//! The config file: which program runs each runtime, and its named
//! permission profiles.
//!
//! The file is `config.toml` in the home, and it is optional. Each runtime
//! has built-in values, and the file changes them per runtime:
//!
//! ```toml
//! [claude]
//! program = "/opt/claude/bin/claude"
//!
//! [claude.profiles]
//! auto = ["--permission-mode", "auto", "--permission-prompts", "none"]
//! review = ["--permission-mode", "plan"]
//! ```
//!
//! A `program` replaces the built-in one. A profile replaces the built-in
//! profile of the same name, and a new name adds a profile. A key the file
//! does not know is an error, so a misspelt setting never goes unused.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use serde::Deserialize;

use crate::runtime::RuntimeName;
use crate::store::Home;

/// The profile a start uses when it names none.
pub const DEFAULT_PROFILE: &str = "auto";

/// The profile that turns every permission check off. It runs only when a
/// start names it.
pub const NO_CHECKS_PROFILE: &str = "no-checks";

/// The settings of every runtime.
#[derive(Debug, Clone)]
pub struct Config {
    claude: RuntimeConfig,
    codex: RuntimeConfig,
}

/// The settings of one runtime.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// The program to run. A bare name is looked up on `PATH`.
    pub program: PathBuf,
    /// The arguments of each permission profile, by name.
    profiles: BTreeMap<String, Vec<String>>,
}

impl RuntimeConfig {
    /// The arguments of the profile `name`.
    pub fn profile(&self, name: &str) -> Result<&[String], String> {
        self.profiles.get(name).map(Vec::as_slice).ok_or_else(|| {
            let known: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
            format!(
                "no permission profile is named {name:?}; the profiles are {}",
                known.join(", ")
            )
        })
    }

    fn apply(mut self, file: RuntimeFile) -> Self {
        if let Some(program) = file.program {
            self.program = program;
        }
        self.profiles.extend(file.profiles);
        self
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    claude: RuntimeFile,
    #[serde(default)]
    codex: RuntimeFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeFile {
    program: Option<PathBuf>,
    #[serde(default)]
    profiles: BTreeMap<String, Vec<String>>,
}

impl Config {
    /// The config of `home`: the built-in values, changed by the file when
    /// it exists.
    pub fn load(home: &Home) -> Result<Self, String> {
        let path = home.config_path();
        let file = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<File>(&text)
                .map_err(|e| format!("cannot parse {}: {e}", path.display()))?,
            Err(e) if e.kind() == ErrorKind::NotFound => File::default(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Ok(Self {
            claude: RuntimeName::Claude.built_in().apply(file.claude),
            codex: RuntimeName::Codex.built_in().apply(file.codex),
        })
    }

    pub fn runtime(&self, name: RuntimeName) -> &RuntimeConfig {
        match name {
            RuntimeName::Claude => &self.claude,
            RuntimeName::Codex => &self.codex,
        }
    }
}

impl RuntimeName {
    /// The settings of this runtime when the file changes none.
    fn built_in(self) -> RuntimeConfig {
        let (program, auto, no_checks): (&str, &[&str], &[&str]) = match self {
            RuntimeName::Claude => (
                "claude",
                &["--permission-mode", "auto", "--permission-prompts", "none"],
                &["--dangerously-skip-permissions"],
            ),
            // These are the settings that `codex exec --approve-for-me` sets.
            // `codex exec resume` has no such flag but takes `-c`, so the
            // settings reach a start and a resume alike.
            RuntimeName::Codex => (
                "codex",
                &[
                    "-c",
                    r#"approvals_reviewer="auto_review""#,
                    "-c",
                    r#"approval_policy="on-request""#,
                    "-c",
                    r#"sandbox_mode="workspace-write""#,
                ],
                &["--dangerously-bypass-approvals-and-sandbox"],
            ),
        };
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect();
        RuntimeConfig {
            program: PathBuf::from(program),
            profiles: BTreeMap::from([
                (DEFAULT_PROFILE.to_string(), args(auto)),
                (NO_CHECKS_PROFILE.to_string(), args(no_checks)),
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config that `text` gives. `tag` keeps each test in its own
    /// directory, since tests run in parallel.
    fn loaded(tag: &str, text: &str) -> Result<Config, String> {
        let root =
            std::env::temp_dir().join(format!("heinzel-config-{tag}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("config.toml"), text).unwrap();
        let config = Config::load(&Home::at(&root));
        fs::remove_dir_all(&root).unwrap();
        config
    }

    /// The file replaces a built-in profile by name and adds new names. The
    /// built-in profiles it does not name stay.
    #[test]
    fn the_file_changes_profiles_by_name() {
        let config = loaded(
            "profiles",
            r#"
[claude]
program = "/opt/claude"
[claude.profiles]
auto = ["--permission-mode", "plan"]
review = ["--permission-mode", "dontAsk"]
"#,
        )
        .unwrap();
        let claude = config.runtime(RuntimeName::Claude);
        assert_eq!(claude.program, PathBuf::from("/opt/claude"));
        assert_eq!(
            claude.profile("auto").unwrap(),
            ["--permission-mode", "plan"]
        );
        assert_eq!(
            claude.profile("review").unwrap(),
            ["--permission-mode", "dontAsk"]
        );
        assert_eq!(
            claude.profile(NO_CHECKS_PROFILE).unwrap(),
            ["--dangerously-skip-permissions"]
        );
        let codex = config.runtime(RuntimeName::Codex);
        assert_eq!(codex.program, PathBuf::from("codex"));
        assert!(codex.profile("review").is_err());
    }

    /// A setting the file misspells is refused, not left unused.
    #[test]
    fn an_unknown_setting_is_an_error() {
        let error = loaded("unknown", "[claude]\nprogramm = \"/opt/claude\"\n").unwrap_err();
        assert!(error.contains("programm"), "{error}");
    }
}
