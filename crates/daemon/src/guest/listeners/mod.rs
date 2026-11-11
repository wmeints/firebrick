//! Reads the TCP ports listening in a running sandbox, so users can see which ports to forward.
//!
//! fbkd runs a short shell script in the guest as root that prints `/proc/net/tcp`,
//! `/proc/net/tcp6` and the socket descriptors of every process, and parses that output itself.
//! It reads procfs rather than running `ss` or `netstat`, so images without them work too. The
//! agent has root in the guest and can change what the script prints, so the output is untrusted:
//! fbkd reads at most [`MAX_OUTPUT`] bytes, waits at most [`TIMEOUT`], skips lines that don't
//! parse and bounds the result.

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
    /// The script exited with an error.
    #[error("the guest script exited with code {0}")]
    Exit(i32),
    /// The exec session ended before the script exited.
    #[error("the guest script ended without an exit status")]
    Incomplete,
    /// The output has no socket table.
    #[error("the guest printed no socket table")]
    NoSocketTable,
}

/// Returns the TCP ports listening in the connected sandbox, sorted by port.
pub async fn read(sb: &Sandbox) -> Result<Vec<Listener>, ListenersError> {
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

    parse_output(&output)
}

/// Returns the listeners in the script's output. Fails when it has no socket table, so a guest
/// whose script printed nothing doesn't look like one where nothing listens.
fn parse_output(output: &[u8]) -> Result<Vec<Listener>, ListenersError> {
    let output = String::from_utf8_lossy(output);

    if !output.lines().any(|line| line == parse::TCP_MARKER) {
        return Err(ListenersError::NoSocketTable);
    }

    Ok(parse::listeners(&output))
}

/// Collects at most [`MAX_OUTPUT`] bytes of the script's stdout until it exits. Past the limit it
/// kills the script and returns what it has.
async fn read_output(handle: &mut ExecHandle, sandbox: &str) -> Result<Vec<u8>, ListenersError> {
    let mut output = Vec::new();

    while let Some(event) = handle.recv().await {
        match event {
            ExecEvent::Stdout(data) if append_bounded(&mut output, &data) => {
                kill(handle, sandbox).await;
                return Ok(output);
            }
            ExecEvent::Exited { code: 0 } => return Ok(output),
            ExecEvent::Exited { code } => return Err(ListenersError::Exit(code)),
            ExecEvent::Failed(failed) => return Err(MicrosandboxError::ExecFailed(failed).into()),
            ExecEvent::Started { .. }
            | ExecEvent::Stdout(_)
            | ExecEvent::Stderr(_)
            | ExecEvent::StdinError(_) => {}
        }
    }

    Err(ListenersError::Incomplete)
}

/// Appends as much of the data as fits in [`MAX_OUTPUT`] bytes, and returns whether the output
/// is full.
fn append_bounded(output: &mut Vec<u8>, data: &[u8]) -> bool {
    let room = MAX_OUTPUT - output.len();
    output.extend_from_slice(&data[..data.len().min(room)]);

    output.len() == MAX_OUTPUT
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

    #[test]
    fn output_is_bounded() {
        let mut output = vec![b'a'; MAX_OUTPUT - 2];

        assert!(!append_bounded(&mut output, b"b"));
        assert!(append_bounded(&mut output, b"cde"));
        assert_eq!(output.len(), MAX_OUTPUT);
        assert!(output.ends_with(b"bc"));
    }

    #[test]
    fn output_without_a_socket_table_fails() {
        let result = parse_output(b"sh: cat: not found\n");

        assert!(matches!(result, Err(ListenersError::NoSocketTable)));
    }

    #[test]
    fn output_with_an_empty_socket_table_has_no_listeners() {
        let listeners = parse_output(b"==tcp==\n==tcp6==\n==procs==\n").unwrap();

        assert_eq!(listeners, []);
    }

    #[test]
    fn output_that_is_not_utf8_still_parses() {
        let mut output = b"==tcp==\n   0: 0100007F:0BB8 00000000:0000 0A 0 0 0 0 0 7\n".to_vec();
        output.extend_from_slice(b"==procs==\npid 3 \xff\xfe\nlrwx 3 -> socket:[7]\n");

        let listeners = parse_output(&output).unwrap();

        assert_eq!(listeners.len(), 1);
        assert_eq!(listeners[0].port, 3000);
        assert_eq!(listeners[0].process.as_ref().unwrap().pid, 3);
    }
}
