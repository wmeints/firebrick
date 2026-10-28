//! The host clipboard through the desktop's own tools: `wl-paste` and `wl-copy` on Wayland,
//! `xclip` on X11, `pbpaste` and `osascript` on macOS and PowerShell on Windows.

use async_trait::async_trait;
use std::ffi::OsString;
use std::io;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use super::{Cause, HostClipboard, HostError, Kind, kinds_where, read_at_most};

/// How long reading or writing the host clipboard may take. The stand-ins give up after 5
/// seconds.
const HOST_TIMEOUT: Duration = Duration::from_secs(4);

/// The most bytes of a host tool's stderr fbkd keeps to log.
const MAX_STDERR: usize = 1024;

/// How long fbkd reads the stderr of a copy tool that failed.
const STDERR_TIMEOUT: Duration = Duration::from_millis(500);

/// The most bytes of the list of types fbkd reads.
const MAX_TYPES: usize = 64 * 1024;

/// A desktop whose clipboard tools [`HostTools`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    /// A Wayland session, read with `wl-paste` and written with `wl-copy`.
    Wayland,
    /// An X11 session, read and written with `xclip`.
    X11,
    /// macOS, read with `pbpaste` and `osascript` and written with `osascript`.
    MacOs,
    /// Windows, read and written with the .NET clipboard in a `powershell.exe` script.
    Windows,
}

/// Reads and writes the host clipboard with the tools of the desktop: `wl-paste` and `wl-copy`
/// on Wayland, `xclip` on X11, `pbpaste` and `osascript` on macOS, and `powershell.exe` on
/// Windows. fbkd inherits the display of the `fbk` that started it, so it uses the clipboard of
/// that desktop session.
#[derive(Debug, Clone)]
pub struct HostTools {
    desktop: Option<Desktop>,
    path: Option<OsString>,
}

impl HostTools {
    /// Returns the tools of the desktop fbkd runs on: macOS, Windows, or on Linux Wayland when
    /// `WAYLAND_DISPLAY` is set and X11 when `DISPLAY` is. They're looked up on fbkd's `PATH`.
    pub fn from_env() -> Self {
        let is_set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());

        Self::new(detect(std::env::consts::OS, is_set), None)
    }

    /// Returns the tools of the desktop, or of none when there's no display, looked up on
    /// `path` instead of fbkd's `PATH` when it's given.
    pub fn new(desktop: Option<Desktop>, path: Option<OsString>) -> Self {
        Self { desktop, path }
    }

    /// Returns the desktop, failing when there's no display.
    fn desktop(&self) -> Result<Desktop, HostError> {
        self.desktop.ok_or_else(|| {
            HostError::Unavailable(
                Cause::NoDisplay,
                "neither WAYLAND_DISPLAY nor DISPLAY is set; start fbkd from your desktop \
                 session by running fbk there first"
                    .to_string(),
            )
        })
    }

    /// Runs the tool, failing when it reports an error.
    async fn run(&self, (program, args): Command, limit: usize) -> Result<Vec<u8>, HostError> {
        let output = run(program, args, self.path.as_ref(), limit).await?;

        if !output.success {
            return Err(failure(program, &output.stderr));
        }

        Ok(output.stdout)
    }
}

#[async_trait]
impl HostClipboard for HostTools {
    async fn kinds(&self) -> Result<Vec<Kind>, HostError> {
        let desktop = self.desktop()?;
        let types = match self.run(list_command(desktop), MAX_TYPES).await {
            Err(HostError::Empty) => return Ok(Vec::new()),
            types => types?,
        };
        let types = String::from_utf8_lossy(&types);

        Ok(match desktop {
            Desktop::MacOs => kinds_from_clipboard_info(&types),
            Desktop::Wayland | Desktop::X11 | Desktop::Windows => kinds_from_types(&types),
        })
    }

    async fn read(&self, kind: Kind, limit: usize) -> Result<Vec<u8>, HostError> {
        if !self.kinds().await?.contains(&kind) {
            return Err(HostError::Empty);
        }

        let desktop = self.desktop()?;
        let output_limit = output_limit(desktop, kind, limit);
        let output = self.run(read_command(desktop, kind), output_limit).await?;

        content(desktop, kind, output, limit)
    }

    async fn write(&self, kind: Kind, content: Vec<u8>) -> Result<(), HostError> {
        let desktop = self.desktop()?;
        let (program, args) = write_command(desktop, kind);

        run_with_input(
            program,
            args,
            self.path.as_ref(),
            input(desktop, kind, content),
        )
        .await
    }
}

/// Returns the desktop of the operating system `os`, as in [`std::env::consts::OS`]: macOS and
/// Windows have one, and on other systems the display variables that `is_set` reports tell,
/// or `None` when there's no display.
fn detect(os: &str, is_set: impl Fn(&str) -> bool) -> Option<Desktop> {
    match os {
        "macos" => Some(Desktop::MacOs),
        "windows" => Some(Desktop::Windows),
        _ if is_set("WAYLAND_DISPLAY") => Some(Desktop::Wayland),
        _ if is_set("DISPLAY") => Some(Desktop::X11),
        _ => None,
    }
}

/// A program and its arguments.
type Command = (&'static str, &'static [&'static str]);

/// The program that runs the Windows clipboard scripts. Windows PowerShell comes with every
/// Windows and runs scripts in the single-threaded apartment the clipboard needs.
const POWERSHELL: &str = "powershell.exe";

/// Lists the kinds on the Windows clipboard as the types [`kinds_from_types`] knows.
const WINDOWS_LIST: &str = "Add-Type -AssemblyName System.Windows.Forms; \
    if ([Windows.Forms.Clipboard]::ContainsText()) { 'text/plain' }; \
    if ([Windows.Forms.Clipboard]::ContainsData('PNG') -or \
        [Windows.Forms.Clipboard]::ContainsImage()) { 'image/png' }";

/// Writes the text on the Windows clipboard to stdout as UTF-8, with LF line endings.
const WINDOWS_READ_TEXT: &str = "Add-Type -AssemblyName System.Windows.Forms; \
    $t = [Windows.Forms.Clipboard]::GetText() -replace '\\r\\n', [string][char]10; \
    $b = [Text.Encoding]::UTF8.GetBytes($t); \
    [Console]::OpenStandardOutput().Write($b, 0, $b.Length)";

/// Writes the image on the Windows clipboard to stdout as PNG: the clipboard's own PNG, or its
/// bitmap encoded as PNG.
const WINDOWS_READ_PNG: &str = "Add-Type -AssemblyName System.Windows.Forms, System.Drawing; \
    $s = [Windows.Forms.Clipboard]::GetData('PNG'); \
    if ($s -is [IO.MemoryStream]) { $b = $s.ToArray() } else { \
        $i = [Windows.Forms.Clipboard]::GetImage(); \
        if ($i -eq $null) { [Console]::Error.Write('Nothing is copied'); exit 1 }; \
        $m = New-Object IO.MemoryStream; \
        $i.Save($m, [Drawing.Imaging.ImageFormat]::Png); $b = $m.ToArray() }; \
    [Console]::OpenStandardOutput().Write($b, 0, $b.Length)";

/// Sets the Windows clipboard to the UTF-8 text on stdin, or clears it for empty text.
const WINDOWS_WRITE_TEXT: &str = "Add-Type -AssemblyName System.Windows.Forms; \
    $m = New-Object IO.MemoryStream; [Console]::OpenStandardInput().CopyTo($m); \
    $t = [Text.Encoding]::UTF8.GetString($m.ToArray()); \
    if ($t) { [Windows.Forms.Clipboard]::SetDataObject($t, $true, 10, 100) } \
    else { [Windows.Forms.Clipboard]::Clear() }";

/// Sets the Windows clipboard to the PNG image on stdin, as PNG for the applications that take
/// it and as a bitmap for the others.
const WINDOWS_WRITE_PNG: &str = "Add-Type -AssemblyName System.Windows.Forms, System.Drawing; \
    $m = New-Object IO.MemoryStream; [Console]::OpenStandardInput().CopyTo($m); \
    $m.Position = 0; $o = New-Object Windows.Forms.DataObject; \
    $o.SetData('PNG', $m); $o.SetImage([Drawing.Image]::FromStream($m)); \
    [Windows.Forms.Clipboard]::SetDataObject($o, $true, 10, 100)";

/// Returns the command that lists the types on the clipboard.
fn list_command(desktop: Desktop) -> Command {
    match desktop {
        Desktop::Wayland => ("wl-paste", &["--list-types"]),
        Desktop::X11 => ("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]),
        Desktop::MacOs => ("osascript", &["-e", "clipboard info"]),
        Desktop::Windows => (
            POWERSHELL,
            &[NO_PROFILE, NON_INTERACTIVE, COMMAND, WINDOWS_LIST],
        ),
    }
}

/// The arguments `powershell.exe` runs a script with: without the user's profile, which could
/// print to stdout or take long, and without prompts.
const NO_PROFILE: &str = "-NoProfile";
/// See [`NO_PROFILE`].
const NON_INTERACTIVE: &str = "-NonInteractive";
/// See [`NO_PROFILE`].
const COMMAND: &str = "-Command";

/// Returns the command that prints the content of the kind.
fn read_command(desktop: Desktop, kind: Kind) -> Command {
    match (desktop, kind) {
        (Desktop::Wayland, Kind::Text) => ("wl-paste", &["--no-newline", "--type", "text"]),
        (Desktop::Wayland, Kind::Png) => ("wl-paste", &["--type", "image/png"]),
        (Desktop::X11, Kind::Text) => (
            "xclip",
            &["-selection", "clipboard", "-t", "UTF8_STRING", "-o"],
        ),
        (Desktop::X11, Kind::Png) => (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
        (Desktop::MacOs, Kind::Text) => ("pbpaste", &[]),
        (Desktop::MacOs, Kind::Png) => ("osascript", &["-e", "the clipboard as «class PNGf»"]),
        (Desktop::Windows, Kind::Text) => (
            POWERSHELL,
            &[NO_PROFILE, NON_INTERACTIVE, COMMAND, WINDOWS_READ_TEXT],
        ),
        (Desktop::Windows, Kind::Png) => (
            POWERSHELL,
            &[NO_PROFILE, NON_INTERACTIVE, COMMAND, WINDOWS_READ_PNG],
        ),
    }
}

/// Returns the command that sets the clipboard to the content of the kind on its stdin.
fn write_command(desktop: Desktop, kind: Kind) -> Command {
    match (desktop, kind) {
        (Desktop::Wayland, Kind::Text) => ("wl-copy", &["--type", "text/plain;charset=utf-8"]),
        (Desktop::Wayland, Kind::Png) => ("wl-copy", &["--type", "image/png"]),
        (Desktop::X11, Kind::Text) => (
            "xclip",
            &["-selection", "clipboard", "-t", "UTF8_STRING", "-i"],
        ),
        (Desktop::X11, Kind::Png) => (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-i"],
        ),
        // osascript reads the script from stdin.
        (Desktop::MacOs, _) => ("osascript", &["-"]),
        (Desktop::Windows, Kind::Text) => (
            POWERSHELL,
            &[NO_PROFILE, NON_INTERACTIVE, COMMAND, WINDOWS_WRITE_TEXT],
        ),
        (Desktop::Windows, Kind::Png) => (
            POWERSHELL,
            &[NO_PROFILE, NON_INTERACTIVE, COMMAND, WINDOWS_WRITE_PNG],
        ),
    }
}

/// Returns the stdin of the [`write_command`] for the content: the content itself, or on macOS
/// an AppleScript that sets the clipboard to it as hex data of the kind's class.
fn input(desktop: Desktop, kind: Kind, content: Vec<u8>) -> Vec<u8> {
    if desktop != Desktop::MacOs {
        return content;
    }

    let class = match kind {
        Kind::Text => "utf8",
        Kind::Png => "PNGf",
    };
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut script = format!("set the clipboard to «data {class}").into_bytes();
    script.reserve(content.len() * 2 + 8);

    for byte in content {
        script.push(DIGITS[usize::from(byte >> 4)]);
        script.push(DIGITS[usize::from(byte & 0x0f)]);
    }

    script.extend_from_slice("»\n".as_bytes());
    script
}

/// Returns the most bytes of output to read for content of at most `limit` bytes. AppleScript
/// prints PNG data as hex.
fn output_limit(desktop: Desktop, kind: Kind, limit: usize) -> usize {
    match (desktop, kind) {
        (Desktop::MacOs, Kind::Png) => limit.saturating_mul(2).saturating_add(64),
        _ => limit,
    }
}

/// Returns the content from the command's output.
fn content(
    desktop: Desktop,
    kind: Kind,
    output: Vec<u8>,
    limit: usize,
) -> Result<Vec<u8>, HostError> {
    let content = match (desktop, kind) {
        (Desktop::MacOs, Kind::Png) => png_from_applescript(&String::from_utf8_lossy(&output))
            .ok_or_else(|| {
                HostError::Unavailable(Cause::Failed, "osascript printed no PNG data".to_string())
            })?,
        _ => output,
    };

    if content.len() > limit {
        return Err(HostError::TooLarge);
    }

    Ok(content)
}

/// Returns the error of a tool that failed: an empty clipboard when its message says so,
/// otherwise the clipboard isn't available.
fn failure(program: &str, stderr: &str) -> HostError {
    let empty = ["Nothing is copied", "No selection", "not available"]
        .iter()
        .any(|message| stderr.contains(message));

    if empty {
        return HostError::Empty;
    }

    HostError::Unavailable(
        Cause::Failed,
        format!(
            "{program} failed: {}",
            firebrick_utils::strip_control(stderr.trim())
        ),
    )
}

/// Returns the kinds of the types that `wl-paste --list-types` or xclip's `TARGETS` lists.
fn kinds_from_types(types: &str) -> Vec<Kind> {
    let types: Vec<&str> = types.lines().map(str::trim).collect();
    let text = types.iter().any(|name| {
        name.to_ascii_lowercase().starts_with("text/plain")
            || ["UTF8_STRING", "STRING", "TEXT"].contains(name)
    });
    let png = types.contains(&"image/png");

    kinds_where(|kind| match kind {
        Kind::Text => text,
        Kind::Png => png,
    })
}

/// Returns the kinds of the classes that AppleScript's `clipboard info` lists.
fn kinds_from_clipboard_info(info: &str) -> Vec<Kind> {
    let text = ["«class utf8»", "Unicode text", "string"]
        .iter()
        .any(|class| info.contains(class));
    let png = info.contains("«class PNGf»");

    kinds_where(|kind| match kind {
        Kind::Text => text,
        Kind::Png => png,
    })
}

/// Decodes the `«data PNGf<hex>»` that AppleScript prints for PNG data.
fn png_from_applescript(output: &str) -> Option<Vec<u8>> {
    let hex = output
        .trim()
        .strip_prefix("«data PNGf")?
        .strip_suffix('»')?
        .as_bytes();

    if hex.len() % 2 != 0 {
        return None;
    }

    hex.chunks(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}

/// The output of a host tool.
struct Output {
    success: bool,
    stdout: Vec<u8>,
    stderr: String,
}

/// Runs the program, looked up on `path` when it's given, and returns its output. Fails with
/// [`HostError::TooLarge`] as soon as it prints more than `limit` bytes, and with
/// [`HostError::Unavailable`] when it can't run or takes longer than [`HOST_TIMEOUT`]. The
/// program is killed when it's given up on.
async fn run(
    program: &str,
    args: &[&str],
    path: Option<&OsString>,
    limit: usize,
) -> Result<Output, HostError> {
    let mut command = command(program, args, path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command.spawn().map_err(|err| spawn_error(program, &err))?;

    tokio::time::timeout(HOST_TIMEOUT, collect(program, child, limit))
        .await
        .map_err(|_| timed_out(program))?
}

/// Runs the program, looked up on `path` when it's given, with the input on its stdin, and
/// waits for it to exit. Fails with [`HostError::Unavailable`] when it can't run, fails, or
/// takes longer than [`HOST_TIMEOUT`], and kills it when it's given up on. Its stdout goes
/// nowhere, and its stderr is only read when it fails: `wl-copy` and `xclip` leave a child
/// behind that serves the clipboard and keeps the pipes it inherited open.
async fn run_with_input(
    program: &str,
    args: &[&str],
    path: Option<&OsString>,
    input: Vec<u8>,
) -> Result<(), HostError> {
    let mut command = command(program, args, path);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let child = command.spawn().map_err(|err| spawn_error(program, &err))?;

    tokio::time::timeout(HOST_TIMEOUT, feed(program, child, input))
        .await
        .map_err(|_| timed_out(program))?
}

/// Returns the command for the program, looked up on `path` when it's given, which kills the
/// program when its child is dropped.
fn command(program: &str, args: &[&str], path: Option<&OsString>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command.args(args).kill_on_drop(true);
    hide_console(&mut command);

    // A PATH set for the child is also where the program is looked up.
    if let Some(path) = path {
        command.env("PATH", path);
    }

    command
}

/// Does nothing: unix tools need no console.
#[cfg(unix)]
fn hide_console(_command: &mut tokio::process::Command) {}

/// Gives the program a console without a window. `fbk` starts fbkd without a console, so
/// Windows would otherwise open a console window for each console program fbkd runs.
#[cfg(windows)]
fn hide_console(command: &mut tokio::process::Command) {
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Returns the error for a program that took longer than [`HOST_TIMEOUT`].
fn timed_out(program: &str) -> HostError {
    let reason = format!("{program} didn't finish within {HOST_TIMEOUT:?}");

    HostError::Unavailable(Cause::Failed, reason)
}

/// Writes the input to the stdin of the program's child, closes it and waits for the child to
/// exit, failing with what it printed on stderr when it doesn't exit successfully.
async fn feed(
    program: &str,
    mut child: tokio::process::Child,
    input: Vec<u8>,
) -> Result<(), HostError> {
    let failed =
        |reason: String| HostError::Unavailable(Cause::Failed, format!("{program} {reason}"));
    let written = match child.stdin.take() {
        // Dropping stdin once the write is done closes it.
        Some(mut stdin) => stdin.write_all(&input).await,
        None => Ok(()),
    };
    let status = child
        .wait()
        .await
        .map_err(|err| failed(format!("failed: {err}")))?;

    if !status.success() {
        let stderr = failure_stderr(child.stderr.take()).await;
        return Err(failed(format!("failed ({status}): {stderr}")));
    }

    written.map_err(|err| failed(format!("can't be written to: {err}")))
}

/// Returns at most [`MAX_STDERR`] bytes of what a program that failed printed on stderr,
/// without control characters. Gives up after [`STDERR_TIMEOUT`], in case a child it left
/// behind keeps the pipe open.
async fn failure_stderr(stderr: Option<tokio::process::ChildStderr>) -> String {
    let read = tokio::time::timeout(STDERR_TIMEOUT, read_at_most(stderr, MAX_STDERR)).await;
    let stderr = match read {
        Ok(Ok(stderr)) => String::from_utf8_lossy(&stderr).into_owned(),
        Ok(Err(err)) => format!("(its stderr can't be read: {err})"),
        Err(_) => "(its stderr didn't end)".to_string(),
    };

    firebrick_utils::strip_control(stderr.trim())
}

/// Returns the error for a program that couldn't be started: a missing tool, or a failure.
fn spawn_error(program: &str, err: &io::Error) -> HostError {
    let cause = match err.kind() {
        io::ErrorKind::NotFound => Cause::NoTool,
        _ => Cause::Failed,
    };

    HostError::Unavailable(cause, format!("{program} can't run: {err}"))
}

/// Reads the output of the program's child and waits for it to exit, failing with
/// [`HostError::TooLarge`] as soon as it prints more than `limit` bytes.
async fn collect(
    program: &str,
    mut child: tokio::process::Child,
    limit: usize,
) -> Result<Output, HostError> {
    let failed =
        |reason: String| HostError::Unavailable(Cause::Failed, format!("{program} {reason}"));
    let (stdout, stderr) = tokio::join!(
        read_at_most(child.stdout.take(), limit.saturating_add(1)),
        read_at_most(child.stderr.take(), MAX_STDERR)
    );
    let stdout = stdout.map_err(|err| failed(format!("can't be read: {err}")))?;

    if stdout.len() > limit {
        return Err(HostError::TooLarge);
    }

    let status = child
        .wait()
        .await
        .map_err(|err| failed(format!("failed: {err}")))?;
    let stderr = match stderr {
        Ok(stderr) => String::from_utf8_lossy(&stderr).into_owned(),
        Err(err) => format!("(its stderr can't be read: {err})"),
    };

    Ok(Output {
        success: status.success(),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_types_recognizes_text_and_png() {
        assert_eq!(
            kinds_from_types("image/png\ntext/plain;charset=utf-8\n"),
            [Kind::Text, Kind::Png]
        );
        assert_eq!(
            kinds_from_types("TARGETS\nUTF8_STRING\nSTRING\n"),
            [Kind::Text]
        );
        assert_eq!(kinds_from_types("image/jpeg\ntext/html\n"), []);
    }

    #[test]
    fn kinds_from_clipboard_info_recognizes_text_and_png() {
        assert_eq!(
            kinds_from_clipboard_info("«class PNGf», 1234, «class utf8», 5, string, 5"),
            [Kind::Text, Kind::Png]
        );
        assert_eq!(
            kinds_from_clipboard_info("«class PNGf», 1234, «class 8BPS», 99"),
            [Kind::Png]
        );
        assert_eq!(kinds_from_clipboard_info(""), []);
    }

    #[test]
    fn png_from_applescript_decodes_the_hex_data() {
        assert_eq!(
            png_from_applescript("«data PNGf89504E47»\n"),
            Some(vec![0x89, 0x50, 0x4e, 0x47])
        );
        assert_eq!(png_from_applescript("«data PNGf895»"), None);
        assert_eq!(png_from_applescript("«data PNGfZZ»"), None);
        assert_eq!(png_from_applescript("text"), None);
    }

    #[test]
    fn detect_follows_the_platform_and_the_display_variables() {
        let set = |names: &'static [&'static str]| move |name: &str| names.contains(&name);

        assert_eq!(detect("macos", set(&[])), Some(Desktop::MacOs));
        assert_eq!(detect("windows", set(&[])), Some(Desktop::Windows));
        assert_eq!(
            detect("linux", set(&["WAYLAND_DISPLAY", "DISPLAY"])),
            Some(Desktop::Wayland)
        );
        assert_eq!(detect("linux", set(&["DISPLAY"])), Some(Desktop::X11));
        assert_eq!(detect("linux", set(&[])), None);
    }

    #[test]
    fn windows_runs_each_clipboard_script_with_powershell_without_a_profile() {
        let commands = [
            list_command(Desktop::Windows),
            read_command(Desktop::Windows, Kind::Text),
            read_command(Desktop::Windows, Kind::Png),
            write_command(Desktop::Windows, Kind::Text),
            write_command(Desktop::Windows, Kind::Png),
        ];

        for (program, args) in commands {
            assert_eq!(program, "powershell.exe");
            assert_eq!(&args[..3], ["-NoProfile", "-NonInteractive", "-Command"]);
            // Double quotes would need escaping on the Windows command line.
            assert!(!args[3].contains('"'), "{}", args[3]);
        }
    }

    #[test]
    fn windows_lists_its_kinds_as_types_and_passes_content_through() {
        assert_eq!(
            kinds_from_types("text/plain\r\nimage/png\r\n"),
            [Kind::Text, Kind::Png]
        );
        assert_eq!(input(Desktop::Windows, Kind::Png, b"png".to_vec()), b"png");
        assert_eq!(output_limit(Desktop::Windows, Kind::Png, 10), 10);
    }

    #[test]
    fn a_tool_that_reports_nothing_copied_means_an_empty_clipboard() {
        let broken = failure("wl-paste", "Failed to connect to a Wayland server\n");

        assert_eq!(failure("wl-paste", "Nothing is copied\n"), HostError::Empty);
        assert_eq!(
            failure("xclip", "Error: target TARGETS not available\n"),
            HostError::Empty
        );
        assert_eq!(
            broken,
            HostError::Unavailable(
                Cause::Failed,
                "wl-paste failed: Failed to connect to a Wayland server".to_string()
            )
        );
    }

    // `HostTools` runs real processes, so `tests/host_tools.rs` tests it in a process of its
    // own: spawning processes here races with the tests in `forward`, which expect the port of
    // a closed listener to be free at once, while a forked child may still hold it.
    #[tokio::test]
    async fn read_at_most_stops_at_the_limit() {
        let output = [0; 100];

        let read = read_at_most(Some(&output[..]), 11).await.unwrap();
        let none = read_at_most(None::<&[u8]>, 11).await.unwrap();

        assert_eq!(read.len(), 11);
        assert!(none.is_empty());
    }
}
