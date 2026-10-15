//! The VT modes of the Windows console, which `fbk run` turns on so keys reach the guest's PTY
//! and its output renders as on a unix terminal.

use crossterm_winapi::{ConsoleMode, Handle};
use windows_sys::Win32::System::Console::{
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
};

/// The console modes before VT input and output were enabled, restored when dropped.
pub struct ConsoleModes(Vec<(ConsoleMode, u32)>);

impl ConsoleModes {
    /// Enables VT input on stdin and VT processing on stdout, where they're consoles. Warns on
    /// stderr when a console refuses the mode, because keys or output then don't come through
    /// as on a unix terminal.
    pub fn enable() -> Self {
        let handles = [
            (Handle::input_handle(), ENABLE_VIRTUAL_TERMINAL_INPUT),
            (Handle::output_handle(), ENABLE_VIRTUAL_TERMINAL_PROCESSING),
        ];

        Self(
            handles
                .into_iter()
                .filter_map(|(handle, flag)| add_flag(ConsoleMode::from(handle.ok()?), flag))
                .collect(),
        )
    }
}

/// Adds the flag to the console's mode, returning the console and its previous mode. A handle
/// without a mode isn't a console, such as a pipe, and is left alone.
fn add_flag(console: ConsoleMode, flag: u32) -> Option<(ConsoleMode, u32)> {
    let mode = console.mode().ok()?;
    if let Err(err) = console.set_mode(mode | flag) {
        eprintln!("warning: can't turn on VT mode {flag:#x} of the console: {err}");
        return None;
    }
    Some((console, mode))
}

impl Drop for ConsoleModes {
    fn drop(&mut self) {
        for (console, mode) in &self.0 {
            restore(console, *mode);
        }
    }
}

/// Gives the console its previous mode back, warning on stderr when it refuses.
fn restore(console: &ConsoleMode, mode: u32) {
    if let Err(err) = console.set_mode(mode) {
        eprintln!("warning: can't restore the console mode {mode:#x}: {err}");
    }
}
