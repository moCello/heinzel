# heinzel

heinzel starts, watches, continues and stops headless agent sessions. It runs
`claude` and `codex`.

A caller names each session by a key. heinzel gives the key no meaning. It
knows keys, runtimes and processes, and nothing about the work.

## Commands

    heinzel start <key> --runtime <claude|codex> --done-file <path> [--cwd <dir>]
                  [--profile <name>] [--model <model>] (--message <text> | --message-file <path>)
    heinzel continue <key> (--message <text> | --message-file <path>)
    heinzel open <key>
    heinzel status <key>
    heinzel wait <key>
    heinzel stop <key>

- `start` opens a new session in `--cwd` (the current directory by default)
  and returns when the agent runs.
- `continue` sends a new message to a session that has stopped, headless.
- `open` resumes the session in this terminal, in the session's directory.
- `status` prints where the session stands. `wait` blocks until the session
  has no writer, then prints the same.
- `stop` ends a headless run: `SIGTERM` to the agent's process group, then
  `SIGKILL` after 10 seconds.

Each command prints the session as one JSON object: its record and its state.
An `open` prints it on the last line, after the agent's own output.

A `continue` on a session whose first run never reported a session id starts
a fresh run under the same key. So a caller can retry a key after a start
that failed, for example on a wrong `program` in the config.

After a lost holder, a `continue` and an `open` are refused. A `stop` ends
the agent that the state names and records `stopped`, and the session is
reachable again. The agent holds a lock of its own, and the stop signals the
agent only while that lock is held. So a process id that the system gave to
another process after the agent ended gets no signal.

## States

| `state` | Meaning |
|---|---|
| `running` | A headless run is in progress. |
| `open` | An interactive open is in progress. |
| `done` | The agent wrote the done file during the run. |
| `question` | The agent finished its turn without the done file. |
| `failed` | The run failed. `reason` says why. |
| `limited` | The account hit a usage limit. `resets_at` is the reset, in Unix seconds, when the runtime said. |
| `stopped` | A `stop` ended the run. |
| `closed` | An open ended without the done file. |

A run ends when the agent process exits. A claude session that waits on a
background task sends a result and lives on, and heinzel keeps it `running`.

`problems` lists what went wrong during a run without ending it, such as a
line of output the adapter could not read.

## One writer

Each session has a writer lock. A detached holder takes it for a headless run,
and `open` takes it for an interactive one. A `continue` or an `open` on a
session that has a writer is refused.

The holder runs in a session of its own. It outlives the shell or the tool
call that started it. The state file comes from the holder alone. When a
reader finds a live state and a free lock, the holder died without a word,
and the reader reports an error.

## Config

heinzel keeps its sessions and its config under `$HEINZEL_HOME`, which
defaults to `~/.heinzel`. The config file `config.toml` is optional:

```toml
[claude]
program = "/opt/claude/bin/claude"

[claude.profiles]
review = ["--permission-mode", "plan"]
```

A permission profile is a named list of CLI arguments for a runtime. Each
headless run of a session uses the profile its start named. Each runtime has
two built-in profiles, and the file can replace them or add more:

| Profile | claude | codex |
|---|---|---|
| `auto` (the default) | `--permission-mode auto --permission-prompts none` | the settings of `--approve-for-me`, as `-c` overrides |
| `no-checks` | `--dangerously-skip-permissions` | `--dangerously-bypass-approvals-and-sandbox` |

`no-checks` runs only when a start names it. An `open` uses no profile: a
person answers the agent's prompts.

The CLI can apply another mode than the one a profile asks for. claude
2.1.285 runs `auto` as `default` on haiku, for example. claude names the mode
it applied at the start of its stream, and a run in another mode lists it in
`problems`. The codex stream names no mode, so heinzel cannot check a codex
profile.

## Usage limits

claude reports a usage limit in its stream, with its reset time. codex
reports only a message in its `--json` stream. heinzel reads the limit and
its reset time from the rate limits that codex records in its session file,
under `$CODEX_HOME/sessions`.

## Development

    make cq        # format check, clippy, tests: every agent is a stub
    make boundary  # the real claude and codex CLIs: uses your usage
