//! The signals that stop the daemon: SIGINT and SIGTERM on Unix, and Ctrl-C, Ctrl-Break, the
//! console closing and Windows shutting down on Windows.

use std::io;

/// Completes on the signals of [`shutdown_signal`] or when `requested` completes.
pub(super) async fn shutdown_or(requested: impl Future<Output = ()>) {
    tokio::select! {
        () = shutdown_signal() => {}
        () = requested => {}
    }
}

/// Completes when SIGINT or SIGTERM is received.
#[cfg(unix)]
async fn shutdown_signal() {
    tokio::select! {
        () = ctrl_c() => {}
        () = sigterm() => {}
    }
    tracing::info!("shutting down");
}

/// Completes when SIGTERM is received.
#[cfg(unix)]
async fn sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut signal) => drop(signal.recv().await),
        Err(err) => not_listening("SIGTERM", &err).await,
    }
}

/// Completes when Ctrl-C or Ctrl-Break is pressed, or the console closes or Windows shuts down.
/// `fbk` starts the daemon without a console, so then only a console it's started from by hand
/// sends these.
#[cfg(windows)]
async fn shutdown_signal() {
    tokio::select! {
        () = ctrl_c() => {}
        () = ctrl_break() => {}
        () = console_close() => {}
        () = windows_shutdown() => {}
    }
    tracing::info!("shutting down");
}

/// Completes when Ctrl-C is pressed, or SIGINT is received on Unix.
async fn ctrl_c() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        not_listening("Ctrl-C", &err).await;
    }
}

/// Completes when Ctrl-Break is pressed.
#[cfg(windows)]
async fn ctrl_break() {
    match tokio::signal::windows::ctrl_break() {
        Ok(mut signal) => drop(signal.recv().await),
        Err(err) => not_listening("Ctrl-Break", &err).await,
    }
}

/// Completes when the console closes.
#[cfg(windows)]
async fn console_close() {
    match tokio::signal::windows::ctrl_close() {
        Ok(mut signal) => drop(signal.recv().await),
        Err(err) => not_listening("the console closing", &err).await,
    }
}

/// Completes when Windows shuts down.
#[cfg(windows)]
async fn windows_shutdown() {
    match tokio::signal::windows::ctrl_shutdown() {
        Ok(mut signal) => drop(signal.recv().await),
        Err(err) => not_listening("Windows shutting down", &err).await,
    }
}

/// Logs that the daemon can't listen for the signal or console event and never completes, so a
/// handler that can't be registered doesn't count as the event.
async fn not_listening(event: &str, err: &io::Error) {
    tracing::warn!("can't listen for {event}, which won't stop the daemon: {err}");
    std::future::pending::<()>().await;
}
