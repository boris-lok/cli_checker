use anyhow::Context;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    // None = kill by signal
    pub exit_code: Option<i32>,
    pub is_timeout: bool,
    pub elapsed: Duration,
}

pub trait CommandExecutor: Send + Sync {
    fn run(&self, command: &str, timeout: Option<Duration>) -> anyhow::Result<CommandOutput>;
}

pub struct ShellCommandExecutor;

impl ShellCommandExecutor {}

impl CommandExecutor for ShellCommandExecutor {
    fn run(&self, command: &str, timeout: Option<Duration>) -> anyhow::Result<CommandOutput> {
        let mut child = shell(command).spawn()?;

        let start = Instant::now();
        let out = spawn_reader(child.stdout.take().expect("stdout is piped"));
        let err = spawn_reader(child.stderr.take().expect("stderr is piped"));

        let (status, is_timeout) = match wait_with_deadline(&mut child, timeout.map(|t| start + t))
        {
            Ok(v) => v,
            Err(e) => {
                kill_tree(&mut child);
                let _ = child.wait();
                return Err(e).context("failed to wait for command");
            }
        };

        let out = collect(out);
        let err = collect(err);

        Ok(CommandOutput {
            exit_code: status.code(),
            is_timeout,
            elapsed: start.elapsed(),
            stdout: out,
            stderr: err,
        })
    }
}

/// Create a new shell command
fn shell(command: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Put the shell in its own process group.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }

    c
}

/// Kill the child and everything in its process group.
///
/// Effective on Unix only. [`shell`] runs the command through `sh -c` and puts
/// that shell in a fresh process group, so the negative-pid `SIGKILL` below
/// reaches the shell and every descendant it spawned. Elsewhere, there is no
/// group to signal and this kills the `sh` process alone: a command that
/// backgrounds work (`cmd &`, a spawned daemon) leaves orphans behind, and
/// because those orphans inherit the stdout/stderr handles, the pipes stay open
/// and [`collect`] can block well past the timeout. Windows would need a job
/// object here, not a signal.
fn kill_tree(child: &mut Child) {
    // A negative pid signals the whole group.
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}

/// Wait for the child, killing it if the deadline passes.
/// Returns the exit status and whether it timed out.
fn wait_with_deadline(
    child: &mut Child,
    deadline: Option<Instant>,
) -> std::io::Result<(ExitStatus, bool)> {
    let mut backoff = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, false));
        }
        if let Some(deadline) = deadline {
            let now = Instant::now();
            if now >= deadline {
                kill_tree(child);
                return Ok((child.wait()?, true));
            }
            backoff = backoff.min(deadline - now);
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(50));
    }
}

/// Read the output of a pipe.
///
/// For a child process, this function will read until EOF.
/// If we wait for a stdout/stderr pipe, it will cause a deadlock if the child process is still running.
/// Moving the read to a separate thread will avoid the deadlock.
fn spawn_reader<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        // On a read error, keep whatever was read so far.
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Convert bytes to a string.
fn collect(handle: std::thread::JoinHandle<Vec<u8>>) -> String {
    handle
        .join()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}
