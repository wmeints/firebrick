//! The VT modes of the Windows console, which `fbk run` turns on so keys reach the guest's PTY
//! and its output renders as on a unix terminal.

use crossterm_winapi::{ConsoleMode, Handle};

/// `ENABLE_VIRTUAL_TERMINAL_INPUT` from the Win32 API: keys arrive as VT sequences.
const VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING` from the Win32 API: VT sequences are rendered.
const VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

/// The console modes before VT input and output were enabled, restored when dropped.
pub struct ConsoleModes(Vec<(ConsoleMode, u32)>);

impl ConsoleModes {
    /// Enables VT input on stdin and VT processing on stdout, where they're consoles.
    pub fn enable() -> Self {
        let handles = [
            (Handle::input_handle(), VIRTUAL_TERMINAL_INPUT),
            (Handle::output_handle(), VIRTUAL_TERMINAL_PROCESSING),
        ];

        Self(
            handles
                .into_iter()
                .filter_map(|(handle, flag)| add_flag(ConsoleMode::from(handle.ok()?), flag))
                .collect(),
        )
    }
}

/// Adds the flag to the console's mode, returning the console and its previous mode.
fn add_flag(console: ConsoleMode, flag: u32) -> Option<(ConsoleMode, u32)> {
    let mode = console.mode().ok()?;
    console.set_mode(mode | flag).ok()?;
    Some((console, mode))
}

impl Drop for ConsoleModes {
    fn drop(&mut self) {
        for (console, mode) in &self.0 {
            let _ = console.set_mode(*mode);
        }
    }
}
