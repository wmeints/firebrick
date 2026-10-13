//! Trusts and installs the mise tools of a sandbox's workspace, so a sandbox comes up with the
//! project's tools ready, and reports the lines mise writes meanwhile as throttled
//! [`MiseUpdate`]s.

use crate::pull::{PROGRESS_INTERVAL, Throttle};
use firebrick_utils::strip_control;
use microsandbox::protocol::exec::{ExecFailed, ExecFailureKind};
use microsandbox::sandbox::exec::{ExecEvent, ExecHandle};
use microsandbox::{MicrosandboxError, Sandbox};
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::time::Duration;
use thiserror::Error;
use tokio::time::Instant;

/// Label that stores whether a sandbox installs its workspace's mise tools on start.
pub const ENABLED_LABEL: &str = "firebrick.mise";

/// The mise config files at the workspace root that fbkd trusts, relative to the workspace.
const CONFIG_FILES: [&str; 5] = [
    "mise.toml",
    ".mise.toml",
    "mise/config.toml",
    ".config/mise.toml",
    ".tool-versions",
];

/// How long `mise install` gets to download and install the tools.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long `mise trust` gets per config file.
const TRUST_TIMEOUT: Duration = Duration::from_secs(60);

/// How many trailing lines of mise's stderr end up in the error.
const STDERR_TAIL_LINES: usize = 10;

/// Progress of installing the mise tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiseUpdate {
    /// The latest line mise wrote, without control characters. Empty on the first update, sent
    /// when the install starts, and on the last one.
    pub line: String,
    /// Whether this is the last update, sent once mise installed the tools.
    pub complete: bool,
}

/// Errors of installing the mise tools.
#[derive(Error, Debug)]
pub enum MiseError {
    /// `mise trust` or `mise install` exited non-zero or timed out.
    #[error("mise install failed in sandbox {sandbox}: {output}")]
    Failed {
        /// Name of the sandbox.
        sandbox: String,
        /// The last lines of mise's stderr, or why it failed when it printed nothing.
        output: String,
    },
    /// The image has no `mise` executable.
    #[error("the image of sandbox {0} has no mise executable")]
    NotInstalled(String),
    /// Running the command in the guest failed.
    #[error("failed to run mise in sandbox {sandbox}")]
    Exec {
        /// Name of the sandbox.
        sandbox: String,
        /// The error of microsandbox.
        #[source]
        source: MicrosandboxError,
    },
}

/// Returns the label value that stores whether mise is enabled.
pub fn label_value(enabled: bool) -> &'static str {
    if enabled { "true" } else { "false" }
}

/// Whether a sandbox with the labels installs its mise tools. Sandboxes without the label were
/// created before it existed and install them.
pub fn is_enabled(labels: &BTreeMap<String, String>) -> bool {
    labels.get(ENABLED_LABEL).map(String::as_str) != Some(label_value(false))
}

/// Returns the guest paths of the mise config files that can exist at the workspace root.
fn config_paths(workspace: &str) -> Vec<String> {
    let workspace = workspace.trim_end_matches('/');
    CONFIG_FILES
        .iter()
        .map(|file| format!("{workspace}/{file}"))
        .collect()
}

/// Returns the last `lines` non-empty lines of `output`.
fn tail(output: &str, lines: usize) -> String {
    let all: Vec<&str> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Trusts each mise config file at the root of `workspace`, then installs the tools, as the
/// image's default user, passing the progress to `on_progress`. Does nothing and reports no
/// progress when the workspace has no mise config file or the image has no mise.
pub async fn install_tools(
    sb: &Sandbox,
    workspace: &str,
    on_progress: impl FnMut(MiseUpdate) + Send,
) -> Result<(), MiseError> {
    let configs = existing_configs(sb, workspace).await?;

    if configs.is_empty() {
        return Ok(());
    }

    let mise = Mise { sb, workspace };
    let mut reporter = Reporter::new(on_progress);

    for config in &configs {
        mise.run(&["trust", config], TRUST_TIMEOUT, &mut reporter)
            .await?;
    }

    mise.run(&["install", "--yes"], INSTALL_TIMEOUT, &mut reporter)
        .await?;
    reporter.complete();

    Ok(())
}

/// Returns the mise config files that exist at the root of the workspace.
async fn existing_configs(sb: &Sandbox, workspace: &str) -> Result<Vec<String>, MiseError> {
    let mut configs = vec![];

    for path in config_paths(workspace) {
        let exists = sb
            .fs()
            .exists(&path)
            .await
            .map_err(|source| exec_error(sb, source))?;

        if exists {
            configs.push(path);
        }
    }

    Ok(configs)
}

/// Runs mise in the workspace of a sandbox.
struct Mise<'a> {
    sb: &'a Sandbox,
    workspace: &'a str,
}

impl Mise<'_> {
    /// Runs mise with the arguments, reporting its output, and fails when it exits non-zero or
    /// runs longer than `timeout`, which kills it.
    async fn run<F: FnMut(MiseUpdate) + Send>(
        &self,
        args: &[&str],
        timeout: Duration,
        reporter: &mut Reporter<F>,
    ) -> Result<(), MiseError> {
        let mut handle = self
            .sb
            .exec_stream_with("mise", |e| e.args(args.iter().copied()).cwd(self.workspace))
            .await
            .map_err(|source| exec_error(self.sb, source))?;
        let mut output = Output::default();
        let followed =
            tokio::time::timeout(timeout, follow(&mut handle, reporter, &mut output)).await;

        let Ok(exit) = followed else {
            kill(self.sb, &handle).await;
            return Err(failed(
                self.sb,
                format!("mise {} timed out after {timeout:?}", args[0]),
            ));
        };

        exit_result(self.sb, exit, &output)
    }
}

/// Reports the output of a mise command until it ends, flushing throttled lines meanwhile.
async fn follow<F: FnMut(MiseUpdate)>(
    handle: &mut ExecHandle,
    reporter: &mut Reporter<F>,
    output: &mut Output,
) -> Exit {
    let mut ticks = tokio::time::interval(PROGRESS_INTERVAL);

    loop {
        tokio::select! {
            event = handle.recv() => {
                if let ControlFlow::Break(exit) = output.handle(event, reporter, Instant::now()) {
                    return exit;
                }
            }
            _ = ticks.tick() => reporter.flush(Instant::now()),
        }
    }
}

/// Kills a mise command that timed out, logging a warning when that fails.
async fn kill(sb: &Sandbox, handle: &ExecHandle) {
    if let Err(err) = handle.kill().await {
        tracing::warn!("failed to kill mise in sandbox {}: {err}", sb.name());
    }
}

/// Turns how a mise command ended into its result.
fn exit_result(sb: &Sandbox, exit: Exit, output: &Output) -> Result<(), MiseError> {
    match exit {
        Exit::Code(0) => Ok(()),
        Exit::Code(code) => Err(failed(sb, failure_output(&output.stderr_bytes, code))),
        Exit::Failed(failure) if failure.kind == ExecFailureKind::NotFound => {
            Err(MiseError::NotInstalled(sb.name().to_string()))
        }
        Exit::Failed(failure) => Err(exec_error(sb, MicrosandboxError::ExecFailed(failure))),
        Exit::Lost => Err(exec_error(
            sb,
            MicrosandboxError::Runtime("exec session ended without exit event".to_string()),
        )),
    }
}

/// How a mise command ended.
#[derive(Debug)]
enum Exit {
    /// It exited with the code.
    Code(i32),
    /// It never ran, for example because the image has no mise.
    Failed(ExecFailed),
    /// The session ended without an exit event.
    Lost,
}

/// The output of one mise command so far.
#[derive(Debug, Default)]
struct Output {
    stdout: LineSplitter,
    stderr: LineSplitter,
    /// All of stderr, for the error when the command fails.
    stderr_bytes: Vec<u8>,
}

impl Output {
    /// Records an event of the command received at `now`, reporting new lines. Breaks with how
    /// the command ended once it did.
    fn handle<F: FnMut(MiseUpdate)>(
        &mut self,
        event: Option<ExecEvent>,
        reporter: &mut Reporter<F>,
        now: Instant,
    ) -> ControlFlow<Exit> {
        match event {
            Some(ExecEvent::Started { .. }) => reporter.begin(),
            Some(ExecEvent::Stdout(data)) => reporter.lines(self.stdout.push(&data), now),
            Some(ExecEvent::Stderr(data)) => {
                self.stderr_bytes.extend_from_slice(&data);
                reporter.lines(self.stderr.push(&data), now);
            }
            Some(ExecEvent::Exited { code }) => return ControlFlow::Break(Exit::Code(code)),
            Some(ExecEvent::Failed(failure)) => return ControlFlow::Break(Exit::Failed(failure)),
            Some(_) => {}
            None => return ControlFlow::Break(Exit::Lost),
        }

        ControlFlow::Continue(())
    }
}

/// Splits chunks of output into lines without control characters, keeping a line that a chunk
/// cuts off until the rest of it arrives.
#[derive(Debug, Default)]
struct LineSplitter {
    partial: Vec<u8>,
}

impl LineSplitter {
    /// Adds a chunk and returns the non-empty lines it completes, trimmed. A carriage return
    /// ends a line too, so a progress line that redraws itself shows each state.
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.partial.extend_from_slice(chunk);

        let Some(end) = self
            .partial
            .iter()
            .rposition(|b| matches!(b, b'\n' | b'\r'))
        else {
            return vec![];
        };
        let complete: Vec<u8> = self.partial.drain(..=end).collect();

        String::from_utf8_lossy(&complete)
            .split(['\n', '\r'])
            .map(|line| strip_control(line).trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }
}

/// Reports the progress of one install: an update when the first mise command starts, the
/// latest line mise wrote at most once every [`PROGRESS_INTERVAL`], and one when it completes.
struct Reporter<F> {
    on_progress: F,
    started: bool,
    pending: Option<String>,
    throttle: Throttle,
}

impl<F: FnMut(MiseUpdate)> Reporter<F> {
    fn new(on_progress: F) -> Self {
        Self {
            on_progress,
            started: false,
            pending: None,
            throttle: Throttle::default(),
        }
    }

    /// Reports that the install started, once.
    fn begin(&mut self) {
        if !self.started {
            self.started = true;
            self.send(String::new(), false);
        }
    }

    /// Keeps the last of the lines and reports it unless a line was reported less than
    /// [`PROGRESS_INTERVAL`] before `now`; [`Self::flush`] reports it later then.
    fn lines(&mut self, lines: Vec<String>, now: Instant) {
        if let Some(line) = lines.into_iter().last() {
            self.begin();
            self.pending = Some(line);
            self.flush(now);
        }
    }

    /// Reports the line that is waiting, when [`PROGRESS_INTERVAL`] passed since the last one.
    fn flush(&mut self, now: Instant) {
        if !self.throttle.ready(now) {
            return;
        }

        if let Some(line) = self.pending.take() {
            self.throttle.sent(now);
            self.send(line, false);
        }
    }

    /// Reports that the install completed, when it reported that it started.
    fn complete(&mut self) {
        if self.started {
            self.send(String::new(), true);
        }
    }

    fn send(&mut self, line: String, complete: bool) {
        (self.on_progress)(MiseUpdate { line, complete });
    }
}

/// Returns the tail of mise's stderr without control characters, or its exit code when it
/// printed nothing.
fn failure_output(stderr: &[u8], code: i32) -> String {
    let stderr = tail(
        &strip_control(&String::from_utf8_lossy(stderr)),
        STDERR_TAIL_LINES,
    );

    if stderr.is_empty() {
        format!("mise exited with code {code}")
    } else {
        stderr
    }
}

fn failed(sb: &Sandbox, output: String) -> MiseError {
    MiseError::Failed {
        sandbox: sb.name().to_string(),
        output,
    }
}

fn exec_error(sb: &Sandbox, source: MicrosandboxError) -> MiseError {
    MiseError::Exec {
        sandbox: sb.name().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn config_paths_lists_root_config_files() {
        assert_eq!(
            config_paths("/workspaces/demo/"),
            [
                "/workspaces/demo/mise.toml",
                "/workspaces/demo/.mise.toml",
                "/workspaces/demo/mise/config.toml",
                "/workspaces/demo/.config/mise.toml",
                "/workspaces/demo/.tool-versions",
            ]
        );
    }

    #[test]
    fn is_enabled_defaults_to_true() {
        assert!(is_enabled(&BTreeMap::new()));
    }

    #[test]
    fn is_enabled_reads_the_label() {
        for enabled in [true, false] {
            let labels = BTreeMap::from([(ENABLED_LABEL.to_string(), label_value(enabled).into())]);

            assert_eq!(is_enabled(&labels), enabled);
        }
    }

    #[test]
    fn tail_keeps_the_last_non_empty_lines() {
        assert_eq!(tail("a\nb\n\nc\nd\n", 3), "b\nc\nd");
        assert_eq!(tail("only\n", 3), "only");
        assert_eq!(tail("", 3), "");
    }

    #[test]
    fn failed_error_names_the_sandbox_and_output() {
        let error = MiseError::Failed {
            sandbox: "demo".into(),
            output: "mise ERROR no such version".into(),
        };

        assert_eq!(
            error.to_string(),
            "mise install failed in sandbox demo: mise ERROR no such version"
        );
    }

    #[test]
    fn failure_output_shows_the_exit_code_without_stderr() {
        assert_eq!(failure_output(b"\n", 3), "mise exited with code 3");
    }

    /// The updates a reporter sent.
    type Updates = Rc<RefCell<Vec<MiseUpdate>>>;

    /// Records the updates of a reporter.
    fn recording() -> (Reporter<impl FnMut(MiseUpdate)>, Updates) {
        let updates = Updates::default();
        let sink = Rc::clone(&updates);
        let reporter = Reporter::new(move |update| sink.borrow_mut().push(update));

        (reporter, updates)
    }

    fn line(line: &str) -> MiseUpdate {
        MiseUpdate {
            line: line.to_string(),
            complete: false,
        }
    }

    fn started() -> MiseUpdate {
        line("")
    }

    fn completed() -> MiseUpdate {
        MiseUpdate {
            line: String::new(),
            complete: true,
        }
    }

    #[test]
    fn line_splitter_joins_lines_split_across_chunks() {
        let mut splitter = LineSplitter::default();

        assert_eq!(splitter.push(b"mise node@22 down"), Vec::<String>::new());
        assert_eq!(
            splitter.push(b"load\nmise node@22 ext"),
            ["mise node@22 download"]
        );
        assert_eq!(splitter.push(b"ract\n"), ["mise node@22 extract"]);
    }

    #[test]
    fn line_splitter_skips_empty_lines_and_splits_on_carriage_returns() {
        let mut splitter = LineSplitter::default();

        assert_eq!(
            splitter.push(b"\n  \nfirst\r\nsecond\r   third  \n"),
            ["first", "second", "third"]
        );
    }

    #[test]
    fn line_splitter_removes_control_characters() {
        let mut splitter = LineSplitter::default();

        assert_eq!(
            splitter.push(b"\x1b[1;32mmise\x1b[0m install\x1b]52;c;ZXZpbA==\x07 done\n"),
            ["[1;32mmise[0m install]52;c;ZXZpbA== done"]
        );
    }

    #[test]
    fn reporter_sends_nothing_before_the_install_starts() {
        let (mut reporter, updates) = recording();

        reporter.complete();

        assert_eq!(*updates.borrow(), vec![]);
    }

    #[test]
    fn reporter_sends_start_lines_and_completion() {
        let (mut reporter, updates) = recording();
        let start = Instant::now();

        reporter.begin();
        reporter.begin();
        reporter.lines(vec!["trusted".into()], start);
        reporter.lines(
            vec!["installing".into(), "downloading".into()],
            start + PROGRESS_INTERVAL,
        );
        reporter.complete();

        assert_eq!(
            *updates.borrow(),
            vec![started(), line("trusted"), line("downloading"), completed()]
        );
    }

    #[test]
    fn reporter_throttles_lines_and_flushes_the_latest_later() {
        let (mut reporter, updates) = recording();
        let start = Instant::now();
        reporter.begin();

        reporter.lines(vec!["first".into()], start);
        reporter.lines(vec!["second".into()], start + PROGRESS_INTERVAL / 4);
        reporter.lines(vec!["third".into()], start + PROGRESS_INTERVAL / 2);
        reporter.flush(start + PROGRESS_INTERVAL / 2);
        reporter.flush(start + PROGRESS_INTERVAL);
        reporter.flush(start + PROGRESS_INTERVAL * 2);

        assert_eq!(
            *updates.borrow(),
            vec![started(), line("first"), line("third")]
        );
    }

    #[test]
    fn output_reports_lines_of_both_streams_and_keeps_stderr() {
        let (mut reporter, updates) = recording();
        let mut output = Output::default();
        let start = Instant::now();

        let events = [
            ExecEvent::Started { pid: 7 },
            ExecEvent::Stdout("out\n".into()),
            ExecEvent::Stderr("err\n".into()),
        ];
        for (step, event) in (0u32..).zip(events) {
            let flow = output.handle(Some(event), &mut reporter, start + PROGRESS_INTERVAL * step);
            assert!(flow.is_continue());
        }

        assert_eq!(*updates.borrow(), vec![started(), line("out"), line("err")]);
        assert_eq!(output.stderr_bytes, b"err\n");
    }

    #[test]
    fn output_reports_the_first_stream_and_drops_a_line_pending_at_exit() {
        let (mut reporter, updates) = recording();
        let mut output = Output::default();
        let start = Instant::now();

        let events = [
            ExecEvent::Started { pid: 7 },
            ExecEvent::Stderr("mise done\n".into()),
            ExecEvent::Stdout("mise installing node\n".into()),
            ExecEvent::Exited { code: 0 },
        ];
        let flows: Vec<_> = events
            .into_iter()
            .map(|event| output.handle(Some(event), &mut reporter, start))
            .collect();
        reporter.complete();

        assert!(matches!(
            flows.last(),
            Some(ControlFlow::Break(Exit::Code(0)))
        ));
        assert_eq!(
            *updates.borrow(),
            vec![started(), line("mise done"), completed()]
        );
    }

    #[test]
    fn output_breaks_with_how_the_command_ended() {
        let (mut reporter, _) = recording();
        let mut output = Output::default();
        let now = Instant::now();

        let exited = output.handle(Some(ExecEvent::Exited { code: 3 }), &mut reporter, now);
        let lost = output.handle(None, &mut reporter, now);

        assert!(matches!(exited, ControlFlow::Break(Exit::Code(3))));
        assert!(matches!(lost, ControlFlow::Break(Exit::Lost)));
    }

    #[test]
    fn output_breaks_when_mise_is_missing() {
        let (mut reporter, updates) = recording();
        let mut output = Output::default();
        let failure = ExecFailed {
            kind: ExecFailureKind::NotFound,
            errno: Some(2),
            errno_name: Some("ENOENT".to_string()),
            message: "mise: not found".to_string(),
            stage: Some("execvp".to_string()),
        };

        let failed = output.handle(
            Some(ExecEvent::Failed(failure)),
            &mut reporter,
            Instant::now(),
        );

        assert!(matches!(
            failed,
            ControlFlow::Break(Exit::Failed(failure)) if failure.kind == ExecFailureKind::NotFound
        ));
        assert_eq!(*updates.borrow(), vec![]);
    }

    #[test]
    fn failure_output_removes_terminal_escape_sequences() {
        let stderr =
            b"mise ERROR task failed\n\x1b]52;c;Y3VybCBldmlsLmV4YW1wbGUgfCBzaA==\x07boom\n";

        assert_eq!(
            failure_output(stderr, 1),
            "mise ERROR task failed\n]52;c;Y3VybCBldmlsLmV4YW1wbGUgfCBzaA==boom"
        );
    }
}
