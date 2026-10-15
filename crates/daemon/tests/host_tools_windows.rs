//! Tests for reading and writing the host clipboard with `HostTools` on Windows, against a stub
//! `powershell.exe` instead of the real clipboard. They mirror the scenarios of
//! `host_tools.rs`, which runs on Unix only.
//!
//! The stub is a .NET console program that Windows PowerShell compiles once per test run. It
//! checks that it's started like the real one, `-NoProfile -NonInteractive -Command <script>`,
//! tells from the script which of the five clipboard scripts it runs (`list`, `text`, `png`,
//! `write-text` or `write-png`), and then does what files next to it say: `<script>.stdout`,
//! `<script>.stderr` and `<script>.exit` set its output and exit code, `<script>.sleep` delays
//! it, a write records its stdin in `<script>.recorded`, unless `<script>.exit-early` makes it
//! exit without reading.
//!
//! The Unix scenarios of a missing tool and a missing display don't apply: PowerShell is part
//! of Windows, which `std::process::Command` also finds through the parent's `PATH`, and Windows
//! always has a desktop. Neither does the one of a copy tool that leaves a child behind, which
//! PowerShell doesn't, or the macOS `osascript` one.
#![cfg(windows)]

use firebrick_daemon::clipboard::{Cause, Desktop, HostClipboard, HostError, HostTools, Kind};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tempfile::TempDir;

/// The source of the stub `powershell.exe`.
const STUB_SOURCE: &str = r#"
using System;
using System.IO;
using System.Threading;

public static class Stub {
    public static int Main(string[] args) {
        if (args.Length != 4 || args[0] != "-NoProfile" || args[1] != "-NonInteractive"
            || args[2] != "-Command") {
            Console.Error.Write("unexpected arguments: " + string.Join(" ", args));
            return 2;
        }
        string script = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, Script(args[3]));
        if (File.Exists(script + ".sleep")) {
            Thread.Sleep(int.Parse(File.ReadAllText(script + ".sleep")));
        }
        bool write = script.Contains("write-");
        if (write && !File.Exists(script + ".exit-early")) {
            MemoryStream input = new MemoryStream();
            Console.OpenStandardInput().CopyTo(input);
            File.WriteAllBytes(script + ".recorded", input.ToArray());
        }
        if (File.Exists(script + ".stdout")) {
            byte[] output = File.ReadAllBytes(script + ".stdout");
            Stream stdout = Console.OpenStandardOutput();
            stdout.Write(output, 0, output.Length);
            stdout.Flush();
        }
        if (File.Exists(script + ".stderr")) {
            Console.Error.Write(File.ReadAllText(script + ".stderr"));
        }
        return File.Exists(script + ".exit") ? int.Parse(File.ReadAllText(script + ".exit")) : 0;
    }

    static string Script(string script) {
        if (script.Contains("ContainsText")) return "list";
        if (script.Contains("GetText")) return "text";
        if (script.Contains("GetData('PNG')")) return "png";
        if (script.Contains("SetDataObject($t")) return "write-text";
        return "write-png";
    }
}
"#;

/// Returns the stub `powershell.exe`, compiling it with Windows PowerShell the first time.
fn compiled_stub() -> &'static Path {
    static STUB: OnceLock<PathBuf> = OnceLock::new();
    STUB.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("fbk-it-{}-powershell-stub", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("stub.cs");
        let exe = dir.join("powershell.exe");
        std::fs::write(&source, STUB_SOURCE).unwrap();
        let compile = format!(
            "Add-Type -Path '{}' -OutputAssembly '{}' -OutputType ConsoleApplication",
            source.display(),
            exe.display()
        );
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &compile])
            .status()
            .unwrap();
        assert!(status.success(), "compiling the stub failed: {status}");
        exe
    })
}

/// Writes the stub into a new directory with the files `(name, content)` that tell it what to
/// do.
fn stub(files: &[(&str, &[u8])]) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::copy(compiled_stub(), dir.path().join("powershell.exe")).unwrap();

    for (name, content) in files {
        std::fs::write(dir.path().join(name), content).unwrap();
    }

    dir
}

/// Returns the Windows tools, with `powershell.exe` looked up in `dir` first.
fn tools(dir: &Path) -> HostTools {
    HostTools::new(Some(Desktop::Windows), Some(dir.as_os_str().into()))
}

/// Returns the cause of an unavailable clipboard, or panics with the result.
fn cause<T: std::fmt::Debug>(result: Result<T, HostError>) -> Cause {
    match result {
        Err(HostError::Unavailable(cause, _)) => cause,
        other => panic!("expected an unavailable clipboard, got {other:?}"),
    }
}

/// What the real list script prints for a clipboard with text and an image.
const TEXT_AND_PNG: &[u8] = b"text/plain\r\nimage/png\r\n";

#[tokio::test]
async fn powershell_lists_and_reads_text_and_png() {
    let dir = stub(&[
        ("list.stdout", TEXT_AND_PNG),
        ("text.stdout", b"hello"),
        ("png.stdout", b"\x89PNG"),
    ]);
    let tools = tools(dir.path());

    assert_eq!(tools.kinds().await, Ok(vec![Kind::Text, Kind::Png]));
    assert_eq!(tools.read(Kind::Text, 100).await, Ok(b"hello".to_vec()));
    assert_eq!(tools.read(Kind::Png, 100).await, Ok(b"\x89PNG".to_vec()));
}

#[tokio::test]
async fn an_empty_clipboard_has_no_kinds_and_no_content() {
    let dir = stub(&[]);
    let tools = tools(dir.path());

    assert_eq!(tools.kinds().await, Ok(vec![]));
    assert_eq!(tools.read(Kind::Text, 100).await, Err(HostError::Empty));
}

#[tokio::test]
async fn a_kind_the_clipboard_lacks_is_empty_without_reading_it() {
    let dir = stub(&[
        ("list.stdout", b"text/plain\r\n"),
        ("png.stderr", b"read a kind the clipboard lacks"),
        ("png.exit", b"3"),
    ]);

    let png = tools(dir.path()).read(Kind::Png, 100).await;

    assert_eq!(png, Err(HostError::Empty));
}

#[tokio::test]
async fn an_image_that_is_gone_when_it_is_read_is_empty() {
    // The real script says so when the clipboard lost its image after it was listed.
    let dir = stub(&[
        ("list.stdout", TEXT_AND_PNG),
        ("png.stderr", b"Nothing is copied"),
        ("png.exit", b"1"),
    ]);

    let png = tools(dir.path()).read(Kind::Png, 100).await;

    assert_eq!(png, Err(HostError::Empty));
}

#[tokio::test]
async fn content_over_the_limit_is_too_large() {
    let large = vec![b'a'; 1_000_000];
    let dir = stub(&[("list.stdout", b"text/plain\r\n"), ("text.stdout", &large)]);

    let text = tools(dir.path()).read(Kind::Text, 10).await;

    assert_eq!(text, Err(HostError::TooLarge));
}

#[tokio::test]
async fn a_failing_tool_is_reported_with_its_message() {
    let dir = stub(&[
        ("list.stderr", b"Add-Type : Cannot add type"),
        ("list.exit", b"1"),
    ]);

    let kinds = tools(dir.path()).kinds().await;

    assert_eq!(
        kinds,
        Err(HostError::Unavailable(
            Cause::Failed,
            "powershell.exe failed: Add-Type : Cannot add type".to_string()
        ))
    );
}

#[tokio::test]
async fn a_tool_that_hangs_is_given_up_on() {
    let dir = stub(&[("list.sleep", b"30000")]);

    let kinds = tools(dir.path()).kinds().await;

    assert_eq!(cause(kinds), Cause::Failed);
}

#[tokio::test]
async fn powershell_writes_text_and_png_each_with_its_own_script() {
    let dir = stub(&[]);
    let tools = tools(dir.path());

    tools.write(Kind::Text, b"hello".to_vec()).await.unwrap();
    tools.write(Kind::Png, b"\x89PNG".to_vec()).await.unwrap();

    let recorded = |script: &str| std::fs::read(dir.path().join(format!("{script}.recorded")));
    assert_eq!(recorded("write-text").unwrap(), b"hello");
    assert_eq!(recorded("write-png").unwrap(), b"\x89PNG");
}

#[tokio::test]
async fn a_failing_copy_tool_is_reported_with_its_message() {
    let dir = stub(&[
        (
            "write-text.stderr",
            b"Requested Clipboard operation did not succeed.",
        ),
        ("write-text.exit", b"1"),
    ]);

    let written = tools(dir.path()).write(Kind::Text, b"hello".to_vec()).await;

    assert_eq!(
        written,
        Err(HostError::Unavailable(
            Cause::Failed,
            "powershell.exe failed (exit code: 1): Requested Clipboard operation did not \
             succeed."
                .to_string()
        ))
    );
}

#[tokio::test]
async fn a_copy_tool_that_fails_before_reading_its_input_is_reported_with_its_message() {
    let dir = stub(&[
        ("write-png.exit-early", b""),
        ("write-png.stderr", b"Add-Type : Cannot add type"),
        ("write-png.exit", b"1"),
    ]);

    let written = tools(dir.path())
        .write(Kind::Png, vec![0x89; 1024 * 1024])
        .await;

    assert_eq!(
        written,
        Err(HostError::Unavailable(
            Cause::Failed,
            "powershell.exe failed (exit code: 1): Add-Type : Cannot add type".to_string()
        ))
    );
}
