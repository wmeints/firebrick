//! Tests for reading the host clipboard with `HostTools`, against stub `wl-paste` and `xclip`
//! scripts instead of a real desktop.
//!
//! They spawn processes, so they live in a test binary of their own: in the unit tests a forked
//! child can briefly hold the port of a listener another test just closed.

use firebrick_daemon::clipboard::{Cause, Desktop, HostClipboard, HostError, HostTools, Kind};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::TempDir;

/// The `wl-paste` of a Wayland clipboard that holds text and a PNG.
const WL_PASTE: &str = r#"case "$*" in
--list-types) printf 'text/plain;charset=utf-8\nimage/png\n' ;;
"--no-newline --type text") printf 'hello' ;;
"--type image/png") printf '\211PNG' ;;
*) exit 2 ;;
esac"#;

/// Writes the stub tools, shell scripts with the bodies, into a new directory.
fn stubs(tools: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();

    for (name, body) in tools {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    dir
}

/// Returns the tools of the desktop, looked up in `dir` first and then in the system's
/// directories, so the stubs can run `head` and `sleep`.
fn tools(desktop: Desktop, dir: &Path) -> HostTools {
    let path = format!("{}:/usr/bin:/bin", dir.display());

    HostTools::new(Some(desktop), Some(path.into()))
}

/// Returns the cause of an unavailable clipboard, or panics with the result.
fn cause<T: std::fmt::Debug>(result: Result<T, HostError>) -> Cause {
    match result {
        Err(HostError::Unavailable(cause, _)) => cause,
        other => panic!("expected an unavailable clipboard, got {other:?}"),
    }
}

#[tokio::test]
async fn wl_paste_lists_and_reads_text_and_png() {
    let dir = stubs(&[("wl-paste", WL_PASTE)]);
    let tools = tools(Desktop::Wayland, dir.path());

    assert_eq!(tools.kinds().await, Ok(vec![Kind::Text, Kind::Png]));
    assert_eq!(tools.read(Kind::Text, 100).await, Ok(b"hello".to_vec()));
    assert_eq!(tools.read(Kind::Png, 100).await, Ok(b"\x89PNG".to_vec()));
}

#[tokio::test]
async fn xclip_lists_and_reads_text_and_png() {
    let xclip = r#"case "$*" in
"-selection clipboard -t TARGETS -o") printf 'TARGETS\nUTF8_STRING\nimage/png\n' ;;
"-selection clipboard -t UTF8_STRING -o") printf 'hello' ;;
"-selection clipboard -t image/png -o") printf '\211PNG' ;;
*) exit 2 ;;
esac"#;
    let dir = stubs(&[("xclip", xclip)]);
    let tools = tools(Desktop::X11, dir.path());

    assert_eq!(tools.kinds().await, Ok(vec![Kind::Text, Kind::Png]));
    assert_eq!(tools.read(Kind::Text, 100).await, Ok(b"hello".to_vec()));
    assert_eq!(tools.read(Kind::Png, 100).await, Ok(b"\x89PNG".to_vec()));
}

#[tokio::test]
async fn an_empty_clipboard_has_no_kinds_and_no_content() {
    let dir = stubs(&[("wl-paste", "echo 'Nothing is copied' >&2; exit 1")]);
    let tools = tools(Desktop::Wayland, dir.path());

    assert_eq!(tools.kinds().await, Ok(vec![]));
    assert_eq!(tools.read(Kind::Text, 100).await, Err(HostError::Empty));
}

#[tokio::test]
async fn a_kind_the_clipboard_lacks_is_empty_without_reading_it() {
    let wl_paste = r#"case "$*" in
--list-types) printf 'text/plain\n' ;;
*) echo 'read a kind the clipboard lacks' >&2; exit 3 ;;
esac"#;
    let dir = stubs(&[("wl-paste", wl_paste)]);

    let png = tools(Desktop::Wayland, dir.path())
        .read(Kind::Png, 100)
        .await;

    assert_eq!(png, Err(HostError::Empty));
}

#[tokio::test]
async fn content_over_the_limit_is_too_large() {
    let wl_paste = r#"case "$*" in
--list-types) printf 'text/plain\n' ;;
*) head -c 1000000 /dev/zero ;;
esac"#;
    let dir = stubs(&[("wl-paste", wl_paste)]);

    let text = tools(Desktop::Wayland, dir.path())
        .read(Kind::Text, 10)
        .await;

    assert_eq!(text, Err(HostError::TooLarge));
}

#[tokio::test]
async fn a_missing_tool_is_reported_as_such() {
    let empty = TempDir::new().unwrap();
    let tools = HostTools::new(
        Some(Desktop::Wayland),
        Some(empty.path().as_os_str().into()),
    );

    assert_eq!(cause(tools.kinds().await), Cause::NoTool);
}

#[tokio::test]
async fn no_display_is_reported_as_such() {
    let tools = HostTools::new(None, None);

    assert_eq!(cause(tools.kinds().await), Cause::NoDisplay);
    assert_eq!(cause(tools.read(Kind::Text, 100).await), Cause::NoDisplay);
}

#[tokio::test]
async fn a_failing_tool_is_reported_with_its_message() {
    let wl_paste = "echo 'Failed to connect to a Wayland server' >&2; exit 1";
    let dir = stubs(&[("wl-paste", wl_paste)]);

    let kinds = tools(Desktop::Wayland, dir.path()).kinds().await;

    assert_eq!(
        kinds,
        Err(HostError::Unavailable(
            Cause::Failed,
            "wl-paste failed: Failed to connect to a Wayland server".to_string()
        ))
    );
}

#[tokio::test]
async fn a_tool_that_hangs_is_given_up_on() {
    let dir = stubs(&[("wl-paste", "exec sleep 30")]);

    let kinds = tools(Desktop::Wayland, dir.path()).kinds().await;

    assert_eq!(cause(kinds), Cause::Failed);
}
