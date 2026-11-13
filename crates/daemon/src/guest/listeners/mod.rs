//! Reads the TCP ports listening in a running sandbox, so users can see which ports to forward.
//!
//! fbkd runs a short shell script in the guest as root that prints `/proc/net/tcp`,
//! `/proc/net/tcp6` and the socket descriptors of every process, and parses that output itself.
//! It reads procfs rather than running `ss` or `netstat`, so images without them work too. The
//! agent has root in the guest and can change what the script prints, so the output is untrusted:
//! fbkd reads at most [`MAX_OUTPUT`] bytes, waits at most [`TIMEOUT`], skips lines that don't
//! parse and bounds the result, and says when it dropped part of it.

use firebrick_utils::strip_control;
use microsandbox::sandbox::exec::{ExecEvent, ExecHandle};
use microsandbox::{MicrosandboxError, Sandbox};
use std::net::IpAddr;
use std::time::Duration;
use thiserror::Error;

mod parse;

/// How long reading the listeners gets in the guest.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// The most bytes of the script's output that are read. The rest is dropped.
pub const MAX_OUTPUT: usize = 1024 * 1024;

/// The most bytes of the script's stderr that are kept to explain a failure.
const MAX_STDERR: usize = 1024;

/// Prints the socket tables, then a `pid <pid> <name>` line for each process, followed by the
/// `ls -l` lines of its `fd` directory. Processes that exit meanwhile are skipped. Uses only
/// `cat` and `ls`, which busybox has too.
const SCRIPT: &str = r#"echo '==tcp=='
cat /proc/net/tcp 2>/dev/null
echo '==tcp6=='
cat /proc/net/tcp6 2>/dev/null
echo '==procs=='
for dir in /proc/[0-9]*; do
  [ -d "$dir/fd" ] || continue
  printf 'pid %s %s\n' "${dir#/proc/}" "$(cat "$dir/comm" 2>/dev/null)"
  ls -l "$dir/fd" 2>/dev/null
done
exit 0
"#;

/// The TCP ports listening in a guest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    /// The listening ports, sorted by port.
    pub listeners: Vec<Listener>,
    /// Whether the guest reported more than fbkd reads, so ports or their processes may be
    /// missing.
    pub truncated: bool,
}

/// A TCP port that listens in the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    /// The port, never 0.
    pub port: u16,
    /// The local addresses the port listens on, sorted.
    pub addresses: Vec<IpAddr>,
    /// The process that owns the socket, the lowest pid when several hold it. `None` when no
    /// process could be found.
    pub process: Option<Process>,
}

/// A guest process, as the guest reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// The guest pid.
    pub pid: u32,
    /// The process name from `/proc/<pid>/comm`, at most 64 characters. It may hold control
    /// characters.
    pub name: String,
}

/// Errors of reading the listeners of a sandbox.
#[derive(Error, Debug)]
pub enum ListenersError {
    /// The script couldn't run, for example because the image has no shell.
    #[error(transparent)]
    Exec(#[from] MicrosandboxError),
    /// The script didn't finish within [`TIMEOUT`].
    #[error("the guest didn't answer within {} seconds", TIMEOUT.as_secs())]
    Timeout,
    /// The script exited with an error. `stderr` is the start of its stderr, trimmed and without
    /// control characters.
    #[error("the guest script exited with code {code}{}", detail(.stderr))]
    Exit {
        /// The exit code.
        code: i32,
        /// What the script wrote to stderr.
        stderr: String,
    },
    /// The exec session ended before the script exited.
    #[error("the guest script ended without an exit status")]
    Incomplete,
    /// The output has no socket table.
    #[error("the guest printed no socket table")]
    NoSocketTable,
}

/// Returns `: <stderr>` for a non-empty stderr, so the exit message names the reason.
fn detail(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

/// Returns the TCP ports listening in the connected sandbox, sorted by port.
pub async fn read(sb: &Sandbox) -> Result<Reading, ListenersError> {
    let mut handle = sb
        .exec_stream_with("sh", |exec| exec.args(["-c", SCRIPT]).user("root").cwd("/"))
        .await?;

    let output = match tokio::time::timeout(TIMEOUT, read_output(&mut handle, sb.name())).await {
        Ok(output) => output?,
        Err(_) => {
            kill(&handle, sb.name()).await;
            return Err(ListenersError::Timeout);
        }
    };

    output.reading()
}

/// What the script printed, at most [`MAX_OUTPUT`] bytes of stdout and [`MAX_STDERR`] bytes of
/// stderr.
#[derive(Default)]
struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Whether stdout reached [`MAX_OUTPUT`], so the rest was dropped.
    truncated: bool,
}

impl Output {
    /// Appends as much of the data as fits in [`MAX_OUTPUT`] bytes, and returns whether stdout
    /// is full.
    fn push_stdout(&mut self, data: &[u8]) -> bool {
        let room = MAX_OUTPUT - self.stdout.len();
        self.stdout.extend_from_slice(&data[..data.len().min(room)]);
        self.truncated = self.stdout.len() == MAX_OUTPUT;

        self.truncated
    }

    /// Keeps the first [`MAX_STDERR`] bytes of stderr.
    fn push_stderr(&mut self, data: &[u8]) {
        let room = MAX_STDERR - self.stderr.len();
        self.stderr.extend_from_slice(&data[..data.len().min(room)]);
    }

    /// Returns the error of a script that exited with the code, with its stderr.
    fn exit_error(&self, code: i32) -> ListenersError {
        let stderr = strip_control(&String::from_utf8_lossy(&self.stderr));

        ListenersError::Exit {
            code,
            stderr: stderr.trim().to_string(),
        }
    }

    /// Returns the listeners in stdout. Fails when it has no socket table, so a guest whose
    /// script printed nothing doesn't look like one where nothing listens.
    fn reading(&self) -> Result<Reading, ListenersError> {
        let stdout = String::from_utf8_lossy(&self.stdout);

        if !stdout.lines().any(|line| line == parse::TCP_MARKER) {
            return Err(ListenersError::NoSocketTable);
        }

        let mut reading = parse::listeners(&stdout);
        reading.truncated |= self.truncated;

        Ok(reading)
    }
}

/// Collects the script's output until it exits. When stdout reaches [`MAX_OUTPUT`], it kills
/// the script and returns what it has.
async fn read_output(handle: &mut ExecHandle, sandbox: &str) -> Result<Output, ListenersError> {
    let mut output = Output::default();

    while let Some(event) = handle.recv().await {
        match event {
            ExecEvent::Stdout(data) if output.push_stdout(&data) => {
                kill(handle, sandbox).await;
                return Ok(output);
            }
            ExecEvent::Stderr(data) => output.push_stderr(&data),
            ExecEvent::Exited { code: 0 } => return Ok(output),
            ExecEvent::Exited { code } => return Err(output.exit_error(code)),
            ExecEvent::Failed(failed) => return Err(MicrosandboxError::ExecFailed(failed).into()),
            ExecEvent::Started { .. } | ExecEvent::Stdout(_) | ExecEvent::StdinError(_) => {}
        }
    }

    Err(ListenersError::Incomplete)
}

/// Kills the script, logging when that fails.
async fn kill(handle: &ExecHandle, sandbox: &str) {
    if let Err(err) = handle.kill().await {
        tracing::warn!("failed to stop the listener script in {sandbox}: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdout(text: &[u8]) -> Output {
        Output {
            stdout: text.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn stdout_is_bounded_and_marked_truncated() {
        let mut output = stdout(&vec![b'a'; MAX_OUTPUT - 2]);

        assert!(!output.push_stdout(b"b"));
        assert!(output.push_stdout(b"cde"));
        assert_eq!(output.stdout.len(), MAX_OUTPUT);
        assert!(output.stdout.ends_with(b"bc"));
        assert!(output.truncated);
    }

    #[test]
    fn truncated_stdout_gives_a_truncated_reading() {
        let mut output = stdout(b"==tcp==\n   0: 0100007F:0BB8 00000000:0000 0A 0 0 0 0 0 7\n");
        output.truncated = true;

        let reading = output.reading().unwrap();

        assert_eq!(reading.listeners.len(), 1);
        assert!(reading.truncated);
    }

    #[test]
    fn exit_error_names_the_stderr_without_control_characters() {
        let mut output = Output::default();
        output.push_stderr(b"ls: /proc/1/fd: Permission denied\x1b[2J\n");
        output.push_stderr(&vec![b'x'; 2 * MAX_STDERR]);

        let err = output.exit_error(2);

        let ListenersError::Exit { code, stderr } = &err else {
            panic!("unexpected error {err:?}");
        };
        assert_eq!(*code, 2);
        assert!(
            stderr.starts_with("ls: /proc/1/fd: Permission denied\nxxx"),
            "{stderr}"
        );
        assert!(stderr.len() < MAX_STDERR, "{}", stderr.len());
        assert!(
            err.to_string().starts_with(
                "the guest script exited with code 2: ls: /proc/1/fd: Permission denied"
            )
        );
    }

    #[test]
    fn exit_error_without_stderr_names_only_the_code() {
        let err = Output::default().exit_error(1);

        assert_eq!(err.to_string(), "the guest script exited with code 1");
    }

    #[test]
    fn output_without_a_socket_table_fails() {
        let result = stdout(b"sh: cat: not found\n").reading();

        assert!(matches!(result, Err(ListenersError::NoSocketTable)));
    }

    #[test]
    fn output_with_an_empty_socket_table_has_no_listeners() {
        let reading = stdout(b"==tcp==\n==tcp6==\n==procs==\n").reading().unwrap();

        assert_eq!(
            reading,
            Reading {
                listeners: vec![],
                truncated: false,
            }
        );
    }

    #[test]
    fn output_that_is_not_utf8_still_parses() {
        let mut text = b"==tcp==\n   0: 0100007F:0BB8 00000000:0000 0A 0 0 0 0 0 7\n".to_vec();
        text.extend_from_slice(b"==procs==\npid 3 \xff\xfe\nlrwx 3 -> socket:[7]\n");

        let reading = stdout(&text).reading().unwrap();

        assert_eq!(
            reading.listeners,
            [Listener {
                port: 3000,
                addresses: vec!["127.0.0.1".parse().unwrap()],
                process: Some(Process {
                    pid: 3,
                    name: "\u{FFFD}\u{FFFD}".to_string(),
                }),
            }]
        );
    }
}
