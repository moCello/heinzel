//! The agent process of a turn, the process group it runs in, and the guard
//! that stops the group when the caller dies.
//!
//! Each turn has a process group of its own, so a signal to the group
//! reaches what the agent started too. A turn ends when the agent exits, or
//! at its deadline: then heinzel sends `SIGTERM` to the group, and `SIGKILL`
//! after [`TERM_GRACE`]. Either way heinzel then kills what is left of the
//! group, so no process in the group outlives the turn.
//!
//! The guard leads the group, and the agent joins it. The guard is
//! `/bin/sh`, so a caller needs no heinzel binary for it. It reads a pipe
//! whose write end only the caller's process holds, and nothing is ever
//! written to that pipe. When the caller dies, for any reason, the kernel
//! closes the write end, and the guard's read ends. The guard then does
//! what the deadline does: `SIGTERM` to the group, and `SIGKILL` after
//! [`TERM_GRACE`]. That `SIGKILL` ends the guard too. The guard ignores
//! `SIGINT`, `SIGHUP`, `SIGQUIT` and `SIGTERM`, and it runs outside the
//! caller's foreground group. So the Ctrl-C that ends the caller does not
//! end the guard.
//!
//! A process that leaves the group, through `setsid` or `setpgid`, gets no
//! signal, from heinzel or from the guard.
//!
//! The group id is the guard's process id. The group keeps that id while
//! the guard lives, and heinzel reaps the guard only after its last signal.
//! So no signal to the group reaches a process that got the same id later.

use std::fs::File;
use std::io::{self, ErrorKind, PipeWriter, Read};
use std::mem;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use heinzel_runtime::Problems;

/// How long a turn's processes have after `SIGTERM`, before `SIGKILL`. A
/// turn gets this grace at its deadline, and when its caller dies.
pub const TERM_GRACE: Duration = Duration::from_secs(5);
/// The longest heinzel waits for output before it looks at the agent again.
const POLL: Duration = Duration::from_millis(100);
/// How much of the end of the agent's stderr a turn keeps.
const STDERR_TAIL: usize = 4096;

/// The program that runs the guard. POSIX systems have it, so the guard
/// needs no program of heinzel's.
const GUARD_SHELL: &str = "/bin/sh";
/// The guard's script. `$1` is the grace period in seconds. stdin is the
/// pipe from the caller, which nobody writes to: `read` returns when the
/// caller's end closes. `kill` to the process id 0 signals the guard's own
/// group, so the last `SIGKILL` ends the guard too.
const GUARD_SCRIPT: &str = "trap '' HUP INT QUIT TERM; read line; kill -s TERM 0; \
sleep \"$1\"; kill -s KILL 0";

// POSIX `sleep` takes whole seconds.
const _: () = assert!(TERM_GRACE.subsec_nanos() == 0);

/// An agent process in a process group of its own, which its guard leads.
/// Dropping it before [`Group::follow`] returns kills the group and reaps
/// the agent and the guard, so no error leaves a process of the turn behind.
pub struct Group {
    child: Child,
    guard: Child,
    /// The process group: the guard's process id.
    id: i32,
    /// The write end of the guard's pipe. Only this process holds it: it is
    /// closed on exec, so no child inherits it. It closes when this process
    /// dies, and the guard then stops the group. Nothing reads or writes the
    /// field: holding it open is its whole job, hence the `_` name.
    _caller: PipeWriter,
    reaped: bool,
}

/// How the agent's run ended.
pub struct Ending {
    pub exit: ExitStatus,
    /// Whether the agent ran past its deadline, and heinzel stopped it.
    pub timed_out: bool,
    /// The end of what the agent wrote to stderr, trimmed.
    pub stderr: String,
}

/// Where the agent stands in its stop.
enum Stop {
    /// It runs, and nothing asked it to stop.
    Running,
    /// It got `SIGTERM`, and it gets `SIGKILL` at this time.
    Terminated(Instant),
    /// It got `SIGKILL`.
    Killed,
}

impl Group {
    /// Start a guard in a new process group, then `command` in that group.
    /// The command reads no stdin, and heinzel reads its stdout and stderr.
    ///
    /// Both start through [`Command::spawn`], which makes only
    /// async-signal-safe calls between fork and exec. That holds in a
    /// caller with many threads.
    pub fn spawn(mut command: Command) -> Result<Self, String> {
        let (reader, writer) =
            io::pipe().map_err(|e| format!("cannot open a pipe for the guard: {e}"))?;
        let mut guard = Command::new(GUARD_SHELL)
            .args([
                "-c",
                GUARD_SCRIPT,
                "heinzel-guard",
                &TERM_GRACE.as_secs().to_string(),
            ])
            .stdin(reader)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("cannot start the guard {GUARD_SHELL}: {e}"))?;
        let id = match i32::try_from(guard.id()) {
            Ok(id) => id,
            Err(_) => {
                let leader = guard.id();
                let _ = guard.kill();
                let _ = guard.wait();
                return Err(format!("the guard's process id {leader} is out of range"));
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(id);
        match command.spawn() {
            Ok(child) => Ok(Self {
                child,
                guard,
                id,
                _caller: writer,
                reaped: false,
            }),
            Err(e) => {
                // The guard stands alone in its group, and nothing is left
                // for it to stop.
                let _ = guard.kill();
                let _ = guard.wait();
                Err(format!(
                    "cannot start {}: {e}",
                    command.get_program().to_string_lossy()
                ))
            }
        }
    }

    /// Follow the agent until it exits, or until heinzel stopped it at
    /// `deadline`. Each line of its stdout goes to `line`, and what cannot be
    /// read goes to `problems`.
    pub fn follow(
        mut self,
        deadline: Instant,
        problems: &mut Problems,
        mut line: impl FnMut(&[u8], &mut Problems),
    ) -> Result<Ending, String> {
        let mut stdout = Pipe::new(self.child.stdout.take().map(OwnedFd::from), "stdout")?;
        let mut stderr = Pipe::new(self.child.stderr.take().map(OwnedFd::from), "stderr")?;
        let mut stop = Stop::Running;
        while !self.exited()? {
            let now = Instant::now();
            let wait = match stop {
                Stop::Running if now >= deadline => {
                    self.signal(libc::SIGTERM)?;
                    let kill_at = now
                        .checked_add(TERM_GRACE)
                        .ok_or("the clock cannot count to the end of the grace period")?;
                    stop = Stop::Terminated(kill_at);
                    TERM_GRACE
                }
                Stop::Running => deadline - now,
                Stop::Terminated(kill_at) if now >= kill_at => {
                    self.signal(libc::SIGKILL)?;
                    stop = Stop::Killed;
                    POLL
                }
                Stop::Terminated(kill_at) => kill_at - now,
                Stop::Killed => POLL,
            };
            wait_for_output(&[&stdout, &stderr], wait)?;
            stdout.read_ready(problems);
            stdout.deliver_lines(problems, &mut line);
            stderr.read_ready(problems);
            stderr.keep_tail();
        }
        // The agent has exited. What it started goes with it, the guard too,
        // and the pipes then hold all the agent wrote.
        self.signal(libc::SIGKILL)?;
        stdout.read_ready(problems);
        stdout.deliver_lines(problems, &mut line);
        stdout.deliver_rest(problems, &mut line);
        stderr.read_ready(problems);
        stderr.keep_tail();
        let exit = self
            .child
            .wait()
            .map_err(|e| format!("cannot reap the agent process: {e}"))?;
        // The group's last signal is sent, so its id may go.
        self.guard
            .wait()
            .map_err(|e| format!("cannot reap the guard process: {e}"))?;
        self.reaped = true;
        Ok(Ending {
            exit,
            timed_out: !matches!(stop, Stop::Running),
            stderr: String::from_utf8_lossy(&stderr.buffer).trim().to_string(),
        })
    }

    /// Whether the agent has exited. It stays unreaped.
    fn exited(&self) -> Result<bool, String> {
        // SAFETY: `siginfo_t` is plain data, for which all zeroes is a valid
        // value.
        let mut info: libc::siginfo_t = unsafe { mem::zeroed() };
        loop {
            // SAFETY: `waitid` writes only into `info`, which lives for the
            // call. `WNOWAIT` leaves the agent unreaped.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != ErrorKind::Interrupted {
                return Err(format!("cannot watch the agent process: {error}"));
            }
        }
        // SAFETY: `waitid` filled `info` in, or left it zero while the agent
        // runs.
        Ok(unsafe { info.si_pid() } != 0)
    }

    /// Send `signal` to the turn's process group. A group with no process
    /// left counts as reached.
    ///
    /// macOS refuses a signal with `EPERM` to a group whose processes have
    /// all exited and wait to be reaped: the agent, and the guard after a
    /// `SIGKILL`. So once the agent has exited, `EPERM` counts as reached
    /// too.
    fn signal(&self, signal: libc::c_int) -> Result<(), String> {
        let group = self.id;
        // SAFETY: `kill` takes plain integers and touches no memory.
        if unsafe { libc::kill(-group, signal) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => return Ok(()),
            Some(libc::EPERM) if self.exited()? => return Ok(()),
            _ => {}
        }
        Err(format!(
            "cannot signal the turn's process group {group}: {error}"
        ))
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // A drop has no caller to tell. The guard is not reaped yet, so the
        // kill goes to a group that is still the turn's. The kill ends the
        // guard too, and the waits reap both.
        let _ = self.signal(libc::SIGKILL);
        let _ = self.child.wait();
        let _ = self.guard.wait();
    }
}

/// One output pipe of the agent, read without blocking.
struct Pipe {
    /// `None` once the pipe ended or failed.
    file: Option<File>,
    name: &'static str,
    buffer: Vec<u8>,
}

impl Pipe {
    fn new(fd: Option<OwnedFd>, name: &'static str) -> Result<Self, String> {
        let file = fd.map(File::from);
        if let Some(file) = &file {
            set_nonblocking(file)
                .map_err(|e| format!("cannot read the agent's {name} without blocking: {e}"))?;
        }
        Ok(Self {
            file,
            name,
            buffer: Vec::new(),
        })
    }

    /// Read all the pipe holds now into the buffer. The end of the pipe, or
    /// an error, closes it.
    fn read_ready(&mut self, problems: &mut Problems) {
        let Some(file) = &mut self.file else {
            return;
        };
        let mut chunk = [0u8; 8192];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    problems.add(format!("cannot read the agent's {}: {e}", self.name));
                    break;
                }
            }
        }
        self.file = None;
    }

    /// Hand each whole line in the buffer to `line`.
    fn deliver_lines(
        &mut self,
        problems: &mut Problems,
        line: &mut impl FnMut(&[u8], &mut Problems),
    ) {
        let mut start = 0;
        while let Some(end) = self.buffer[start..].iter().position(|&byte| byte == b'\n') {
            line(&self.buffer[start..start + end + 1], problems);
            start += end + 1;
        }
        self.buffer.drain(..start);
    }

    /// Hand a last line that has no line end to `line`.
    fn deliver_rest(
        &mut self,
        problems: &mut Problems,
        line: &mut impl FnMut(&[u8], &mut Problems),
    ) {
        if !self.buffer.is_empty() {
            line(&self.buffer, problems);
            self.buffer.clear();
        }
    }

    /// Drop all but the last [`STDERR_TAIL`] bytes of the buffer.
    fn keep_tail(&mut self) {
        let excess = self.buffer.len().saturating_sub(STDERR_TAIL);
        self.buffer.drain(..excess);
    }
}

fn set_nonblocking(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: `fcntl` reads and sets the flags of a descriptor that `file`
    // holds open, and touches no memory.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above.
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Wait until a pipe that is still open holds output or ends, or until
/// `limit` passed, whichever comes first. The wait lasts at most [`POLL`].
fn wait_for_output(pipes: &[&Pipe], limit: Duration) -> Result<(), String> {
    let wait = limit.min(POLL);
    let mut fds: Vec<libc::pollfd> = pipes
        .iter()
        .filter_map(|pipe| pipe.file.as_ref())
        .map(|file| libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    if fds.is_empty() {
        thread::sleep(wait);
        return Ok(());
    }
    // `wait` is at most `POLL`, so its milliseconds fit. A part of a
    // millisecond rounds up, so a short wait still waits.
    let millis = libc::c_int::try_from(wait.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX);
    // `fds` holds one entry per pipe: two at most, a count every `nfds_t`
    // holds.
    let count = fds.len() as libc::nfds_t;
    // SAFETY: `fds` is a live array of `count` entries, and `poll` writes
    // only their `revents`.
    if unsafe { libc::poll(fds.as_mut_ptr(), count, millis) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(format!("cannot wait for the agent's output: {error}"));
        }
    }
    Ok(())
}
