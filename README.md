# heinzel

heinzel starts, watches, continues and stops headless agent sessions. It runs
`claude` and `codex`. The binary serves a long run that a caller checks on
later. The library runs one turn for a caller that waits for it: see
[Library](#library).

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

## Library

`heinzel::run` runs one turn and returns when it ends. A `heinzel::Turn`
names all of it:

- the runtime and the program to run,
- the directory the agent runs in,
- the session: a new one, or a resume of a named one. claude takes an id
  for a new session. codex chooses its own, and a turn that names one is
  refused.
- the model, or none for the CLI's default,
- the arguments that set what the agent may do. heinzel puts them on the
  command line as given, and adds no permission of its own.
- the time limit. It must be above zero. At the limit, heinzel sends
  `SIGTERM` to the turn's process group, and `SIGKILL` 5 seconds later.
- the message.

The report says that the turn finished, with the session id and the agent's
final answer. Or it says that the turn hit a usage limit, with the reset when
the runtime names it. Or it says that the turn failed, and why. Its
`problems` list what went wrong without ending the turn.

Each turn runs in a process group of its own. When the agent exits, heinzel
kills what is left of the group, so no process in the group outlives the
turn.

A turn also stops when its caller dies, for any reason: a Ctrl-C at the
caller's terminal, a `SIGKILL`, a crash. A guard process leads the turn's
group. It is `/bin/sh`, so the caller needs no heinzel binary. The guard
reads a pipe whose write end only the caller's process holds. When the
caller dies, the pipe closes, and the guard sends the group `SIGTERM`, then
`SIGKILL` 5 seconds later. The guard ignores the Ctrl-C that ends the caller.
The caller does nothing for any of this.

A process that leaves the group through `setsid` or `setpgid` gets no
signal, from heinzel or from the guard.

A turn writes nothing under `$HEINZEL_HOME`. `heinzel::version_note` says
when the installed CLI is not the version heinzel was validated against.

## Development

    make cq        # format check, clippy, tests: every agent is a stub
    make boundary  # the real claude and codex CLIs: uses your usage

heinzel runs on any version of claude and codex. On a version it was not
validated against, it warns. A version is validated when the boundary test
of its CLI passes on it. When every boundary test passes, `make boundary`
writes the version of each CLI on `PATH` to `runtime/validated/<runtime>`,
and the next build compiles it in. So a new CLI release needs a boundary run
and a commit of those files, not a change to the code.

### Validate a new CLI release

    cargo xtask validate

The command reads the version of each CLI on `PATH`, and skips a CLI that
is not on `PATH`. It runs the boundary test only of a CLI whose version
`runtime/validated/` does not hold. When
every test it ran passes, it writes the new versions there. When the record
holds every installed version, it runs no test and uses no usage. It asks
nothing, so a job can run it unattended. It never commits or pushes: the
caller commits the record.

| Exit | Meaning |
|---|---|
| 0 | The record holds the version of every installed CLI. stdout lists each record file the command changed, one path per line. |
| 1 | A boundary test failed. The record is as it was. stderr names the CLI and the test. |
| 2 | The command could not do its check, for example on a CLI whose `--version` fails. The record is as it was, unless stderr names the record files the command replaced before an error. |

Any other exit code means that cargo could not build or run the command.

A test that fails stops the run, and the tests after it do not run. The
record changes only when every test that ran passed. `make boundary` runs
`cargo xtask validate --all`, which runs the test of every installed CLI.
