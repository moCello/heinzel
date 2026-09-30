//! What a stub prints for `--version`: the output of each validated CLI, and
//! of a claude that is not validated.

use heinzel_runtime::RuntimeName;

/// What the validated claude prints for `--version`.
pub fn claude_version() -> String {
    format!("{} (Claude Code)", validated(RuntimeName::Claude))
}

/// What the validated codex prints for `--version`.
pub fn codex_version() -> String {
    format!("codex-cli {}", validated(RuntimeName::Codex))
}

/// A claude version that is never the validated one, and what that claude
/// prints for `--version`. Its major version is one past the validated one,
/// so neither version holds the other as a substring.
pub fn other_claude_version() -> (String, String) {
    let validated = validated(RuntimeName::Claude);
    let major: u64 = validated.split('.').next().unwrap().parse().unwrap();
    let other = format!("{}.0.0", major + 1);
    let output = format!("{other} (Claude Code)");
    (other, output)
}

/// The version the adapter of `runtime` was validated against.
fn validated(runtime: RuntimeName) -> &'static str {
    runtime.adapter().validated_version()
}
