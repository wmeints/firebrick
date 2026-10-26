//! Shows the progress of the image pull while fbkd creates a sandbox, and of installing the
//! workspace's mise tools while it starts one: a progress bar or spinner on a terminal, and
//! otherwise a line when the step starts and one when it ends. Shows nothing for a step that
//! sends no progress, for example because the image is cached or the workspace has no mise
//! config.

use std::fmt;
use std::io::{self, IsTerminal, Stderr, Write};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

use crate::api::{ImagePullProgress, MiseInstallProgress};

/// Template of the bar when the size of the image is known.
const BAR_TEMPLATE: &str =
    "Pulling {msg} [{bar:30}] {binary_bytes} / {binary_total_bytes} ({percent}%)";

/// Template of the bar when the manifest has no layer sizes.
const BYTES_TEMPLATE: &str = "Pulling {msg} {binary_bytes}";

/// Template of the spinner while mise installs the tools. `{wide_msg}` holds the latest line of
/// mise and is truncated to fit the terminal, so the spinner stays on one line.
const MISE_TEMPLATE: &str = "{spinner} Installing mise tools ({elapsed}){wide_msg}";

/// How often the spinner moves and its elapsed time updates.
const SPINNER_TICK: Duration = Duration::from_millis(100);

/// Where the progress of a pull or mise install goes.
enum Output<W> {
    /// A progress bar, updated in place. It stays hidden until the pull starts and the target
    /// is taken, so it's never drawn without its style.
    Bar {
        bar: ProgressBar,
        target: Option<ProgressDrawTarget>,
    },
    /// A line when the pull starts and one when it ends.
    Lines(W),
}

/// Shows the progress of an image pull from the progress messages of a start stream. An
/// unfinished bar is cleared when the view is dropped, so an error shows on a clean line.
pub struct PullProgressView<W: Write = Stderr> {
    output: Output<W>,
    image: Option<String>,
    finished: bool,
}

impl PullProgressView {
    /// Returns a view that shows a bar when stderr is a terminal, and lines otherwise.
    pub fn stderr() -> Self {
        Self::new(Output::stderr())
    }
}

impl Output<Stderr> {
    /// Returns a bar on stderr when it is a terminal, and lines on stderr otherwise.
    fn stderr() -> Self {
        if io::stderr().is_terminal() {
            Self::bar(ProgressDrawTarget::stderr())
        } else {
            Self::Lines(io::stderr())
        }
    }
}

impl<W> Output<W> {
    /// Returns a bar that draws on the target once the pull starts.
    fn bar(target: ProgressDrawTarget) -> Self {
        Self::Bar {
            bar: ProgressBar::hidden(),
            target: Some(target),
        }
    }
}

impl<W: Write> PullProgressView<W> {
    fn new(output: Output<W>) -> Self {
        Self {
            output,
            image: None,
            finished: false,
        }
    }

    /// Shows a progress message, and finishes the pull when it is the last one.
    pub fn update(&mut self, progress: &ImagePullProgress) {
        if self.finished {
            return;
        }

        if self.image.is_none() {
            self.begin(progress);
        }

        if let Output::Bar { bar, .. } = &self.output {
            bar.set_position(progress.downloaded_bytes);
        }

        if progress.complete {
            self.finish();
        }
    }

    /// Finishes a pull that is being shown. Does nothing when no pull was shown.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }

        let Some(image) = &self.image else {
            return;
        };

        match &mut self.output {
            Output::Bar { bar, .. } => bar.finish(),
            // A failed write to stderr leaves nowhere to report it.
            Output::Lines(writer) => drop(writeln!(writer, "Pulled image {image}")),
        }

        self.finished = true;
    }

    /// Starts showing the pull of the image in the message.
    fn begin(&mut self, progress: &ImagePullProgress) {
        let image = progress.image.clone();

        match &mut self.output {
            Output::Bar { bar, target } => show_bar(bar, target.take(), progress),
            Output::Lines(writer) => drop(writeln!(writer, "Pulling image {image}...")),
        }

        self.image = Some(image);
    }
}

/// Gives the bar the template that fits the progress, with the image as its message, then
/// shows it on the target.
fn show_bar(bar: &ProgressBar, target: Option<ProgressDrawTarget>, progress: &ImagePullProgress) {
    let template = match progress.total_bytes {
        Some(total) => {
            bar.set_length(total);
            BAR_TEMPLATE
        }
        None => BYTES_TEMPLATE,
    };

    // The templates are constants with valid keys, so parsing them can't fail.
    if let Ok(style) = ProgressStyle::with_template(template) {
        bar.set_style(style.progress_chars("=> "));
    }

    bar.set_message(progress.image.clone());

    if let Some(target) = target {
        bar.set_draw_target(target);
    }
}

/// Shows the progress of installing the mise tools from the progress messages of a start
/// stream. The spinner is cleared when the install ends or the view is dropped, so an error
/// shows on a clean line.
pub struct MiseProgressView<W: Write = Stderr> {
    output: Output<W>,
    started: bool,
    finished: bool,
}

impl MiseProgressView {
    /// Returns a view that shows a spinner when stderr is a terminal, and lines otherwise.
    pub fn stderr() -> Self {
        Self::new(Output::stderr())
    }
}

impl<W: Write> MiseProgressView<W> {
    fn new(output: Output<W>) -> Self {
        Self {
            output,
            started: false,
            finished: false,
        }
    }

    /// Shows a progress message, and finishes the install when it is the last one. A warning
    /// is printed on a line of its own, which stays when the spinner is cleared.
    pub fn update(&mut self, progress: &MiseInstallProgress) {
        if self.finished {
            return;
        }

        if !self.started {
            self.begin();
        }

        if !progress.warning.is_empty() {
            self.warn(&progress.warning);
        }

        if let (Output::Bar { bar, .. }, false) = (&self.output, progress.line.is_empty()) {
            bar.set_message(format!(": {}", progress.line));
        }

        if progress.complete {
            self.finish();
        }
    }

    /// Finishes an install that is being shown. Does nothing when no install was shown.
    pub fn finish(&mut self) {
        if self.finished || !self.started {
            return;
        }

        match &mut self.output {
            Output::Bar { bar, .. } => bar.finish_and_clear(),
            // A failed write to stderr leaves nowhere to report it.
            Output::Lines(writer) => drop(writeln!(writer, "Installed mise tools")),
        }

        self.finished = true;
    }

    fn warn(&mut self, warning: &str) {
        match &mut self.output {
            Output::Bar { bar, .. } => bar.println(format!("warning: {warning}")),
            // A failed write to stderr leaves nowhere to report it.
            Output::Lines(writer) => drop(writeln!(writer, "warning: {warning}")),
        }
    }

    /// Starts showing the install, timing it from now.
    fn begin(&mut self) {
        match &mut self.output {
            Output::Bar { bar, target } => show_spinner(bar, target.take()),
            Output::Lines(writer) => drop(writeln!(writer, "Installing mise tools...")),
        }

        self.started = true;
    }
}

/// Gives the bar the spinner style, then shows it on the target and keeps it moving.
fn show_spinner(bar: &ProgressBar, target: Option<ProgressDrawTarget>) {
    // The template is a constant with valid keys, so parsing it can't fail.
    if let Ok(style) = ProgressStyle::with_template(MISE_TEMPLATE) {
        bar.set_style(style.with_key("elapsed", write_elapsed));
    }

    bar.reset_elapsed();

    if let Some(target) = target {
        bar.set_draw_target(target);
    }

    bar.tick();
    bar.enable_steady_tick(SPINNER_TICK);
}

/// Writes the time the bar has been running, such as `1m12s`.
fn write_elapsed(state: &ProgressState, writer: &mut dyn fmt::Write) {
    // A failed write only leaves the elapsed time out of the line.
    let _ = write!(writer, "{}", compact_duration(state.elapsed()));
}

/// Formats a duration in whole seconds as `12s`, `1m12s` or `1h02m03s`.
fn compact_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);

    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m{seconds:02}s"),
        _ => format!("{hours}h{minutes:02}m{seconds:02}s"),
    }
}

impl<W: Write> Drop for MiseProgressView<W> {
    /// Clears a spinner that didn't finish, for example because the install failed.
    fn drop(&mut self) {
        if let Output::Bar { bar, .. } = &self.output {
            bar.finish_and_clear();
        }
    }
}

impl<W: Write> Drop for PullProgressView<W> {
    /// Clears a bar that didn't finish, for example because the pull failed.
    fn drop(&mut self) {
        if let (Output::Bar { bar, .. }, Some(_), false) =
            (&self.output, &self.image, self.finished)
        {
            bar.finish_and_clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::{InMemoryTerm, TermLike};
    use std::sync::{Arc, Mutex};

    /// A terminal that records every string written to it.
    #[derive(Debug, Clone, Default)]
    struct RecordingTerm(Arc<Mutex<Vec<String>>>);

    impl TermLike for RecordingTerm {
        fn width(&self) -> u16 {
            120
        }

        fn move_cursor_up(&self, _: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_down(&self, _: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_right(&self, _: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _: usize) -> io::Result<()> {
            Ok(())
        }

        fn write_line(&self, s: &str) -> io::Result<()> {
            self.write_str(s)
        }

        fn write_str(&self, s: &str) -> io::Result<()> {
            self.0.lock().unwrap().push(s.to_string());
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    const IMAGE: &str = "ghcr.io/wmeints/firebrick-base:test";
    const MIB: u64 = 1024 * 1024;

    fn progress(downloaded_mib: u64, total_mib: Option<u64>) -> ImagePullProgress {
        ImagePullProgress {
            image: IMAGE.to_string(),
            downloaded_bytes: downloaded_mib * MIB,
            total_bytes: total_mib.map(|total| total * MIB),
            complete: false,
        }
    }

    fn completed(downloaded_mib: u64, total_mib: Option<u64>) -> ImagePullProgress {
        ImagePullProgress {
            complete: true,
            ..progress(downloaded_mib, total_mib)
        }
    }

    fn terminal_view() -> (PullProgressView<Vec<u8>>, InMemoryTerm) {
        let term = InMemoryTerm::new(4, 120);
        let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
        let view = PullProgressView::new(Output::bar(target));

        (view, term)
    }

    fn lines(messages: &[ImagePullProgress], finish: bool) -> String {
        let mut view = PullProgressView::new(Output::Lines(Vec::new()));

        for message in messages {
            view.update(message);
        }

        if finish {
            view.finish();
        }

        let Output::Lines(written) = &view.output else {
            unreachable!("the view writes lines");
        };

        String::from_utf8(written.clone()).unwrap()
    }

    #[test]
    fn bar_shows_downloaded_and_total_bytes_with_a_percentage() {
        let (mut view, term) = terminal_view();

        view.update(&progress(100, Some(400)));

        let contents = term.contents();
        assert!(
            contents.starts_with(&format!("Pulling {IMAGE} [")),
            "{contents}"
        );
        assert!(
            contents.ends_with("100.00 MiB / 400.00 MiB (25%)"),
            "{contents}"
        );
    }

    #[test]
    fn bar_is_only_drawn_with_its_style() {
        let term = RecordingTerm::default();
        let target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let mut view = PullProgressView::<Vec<u8>>::new(Output::bar(target));

        view.update(&progress(100, Some(400)));
        view.update(&completed(400, Some(400)));

        let frames = term.0.lock().unwrap().clone();
        let drawn: Vec<_> = frames.iter().filter(|s| !s.trim().is_empty()).collect();
        assert!(!drawn.is_empty());
        assert!(
            drawn
                .iter()
                .all(|frame| frame.starts_with(&format!("Pulling {IMAGE} ["))),
            "{drawn:?}"
        );
    }

    #[test]
    fn bar_shows_downloaded_bytes_only_without_a_total() {
        let (mut view, term) = terminal_view();

        view.update(&progress(100, None));

        assert_eq!(term.contents(), format!("Pulling {IMAGE} 100.00 MiB"));
    }

    #[test]
    fn bar_stays_finished_when_the_pull_completes() {
        let (mut view, term) = terminal_view();

        view.update(&progress(100, Some(400)));
        view.update(&completed(400, Some(400)));
        drop(view);

        let contents = term.contents();
        assert!(
            contents.ends_with("400.00 MiB / 400.00 MiB (100%)"),
            "{contents}"
        );
    }

    #[test]
    fn bar_ends_at_the_total_when_the_sandbox_starts_without_a_last_message() {
        let (mut view, term) = terminal_view();

        view.update(&progress(100, Some(400)));
        view.finish();
        drop(view);

        let contents = term.contents();
        assert!(
            contents.ends_with("400.00 MiB / 400.00 MiB (100%)"),
            "{contents}"
        );
    }

    #[test]
    fn bar_is_cleared_when_the_pull_fails() {
        let (mut view, term) = terminal_view();

        view.update(&progress(100, Some(400)));
        drop(view);

        assert_eq!(term.contents(), "");
    }

    #[test]
    fn bar_shows_nothing_without_progress() {
        let (mut view, term) = terminal_view();

        view.finish();
        drop(view);

        assert_eq!(term.contents(), "");
    }

    #[test]
    fn lines_show_the_start_and_end_of_the_pull_only() {
        let messages = [
            progress(100, Some(400)),
            progress(200, Some(400)),
            completed(400, Some(400)),
            // Messages after the last one are ignored.
            progress(400, Some(400)),
        ];

        assert_eq!(
            lines(&messages, true),
            format!("Pulling image {IMAGE}...\nPulled image {IMAGE}\n")
        );
    }

    #[test]
    fn lines_end_the_pull_when_the_sandbox_starts_before_it_completed() {
        assert_eq!(
            lines(&[progress(100, None)], true),
            format!("Pulling image {IMAGE}...\nPulled image {IMAGE}\n")
        );
    }

    #[test]
    fn lines_show_nothing_without_progress() {
        assert_eq!(lines(&[], true), "");
    }

    fn mise_line(line: &str) -> MiseInstallProgress {
        MiseInstallProgress {
            line: line.to_string(),
            ..MiseInstallProgress::default()
        }
    }

    fn mise_completed() -> MiseInstallProgress {
        MiseInstallProgress {
            complete: true,
            ..MiseInstallProgress::default()
        }
    }

    fn mise_warning(warning: &str) -> MiseInstallProgress {
        MiseInstallProgress {
            warning: warning.to_string(),
            ..MiseInstallProgress::default()
        }
    }

    #[test]
    fn mise_lines_show_a_warning_on_a_line_of_its_own() {
        let messages = [
            mise_line(""),
            mise_warning("couldn't apply the dotfiles: boom"),
            mise_completed(),
        ];

        assert_eq!(
            mise_lines(&messages, true),
            "Installing mise tools...\nwarning: couldn't apply the dotfiles: boom\nInstalled mise tools\n"
        );
    }

    #[test]
    fn spinner_keeps_a_warning_when_the_install_completes() {
        let (mut view, term) = mise_terminal_view(120);

        view.update(&mise_line(""));
        view.update(&mise_warning("couldn't apply the dotfiles: boom"));
        view.update(&mise_completed());

        assert_eq!(
            term.contents(),
            "warning: couldn't apply the dotfiles: boom"
        );
    }

    fn mise_terminal_view(width: u16) -> (MiseProgressView<Vec<u8>>, InMemoryTerm) {
        let term = InMemoryTerm::new(4, width);
        let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
        let view = MiseProgressView::new(Output::bar(target));

        (view, term)
    }

    /// Returns the terminal contents without the spinner character in front.
    fn spinner_line(term: &InMemoryTerm) -> String {
        let contents = term.contents();
        let (_spinner, line) = contents.split_once(' ').unwrap_or_default();

        line.to_string()
    }

    fn mise_lines(messages: &[MiseInstallProgress], finish: bool) -> String {
        let mut view = MiseProgressView::new(Output::Lines(Vec::new()));

        for message in messages {
            view.update(message);
        }

        if finish {
            view.finish();
        }

        let Output::Lines(written) = &view.output else {
            unreachable!("the view writes lines");
        };

        String::from_utf8(written.clone()).unwrap()
    }

    #[test]
    fn spinner_shows_the_elapsed_time_when_the_install_starts() {
        let (mut view, term) = mise_terminal_view(120);

        view.update(&mise_line(""));

        assert_eq!(spinner_line(&term), "Installing mise tools (0s)");
    }

    #[test]
    fn spinner_shows_the_latest_line_of_mise() {
        let (mut view, term) = mise_terminal_view(120);

        view.update(&mise_line(""));
        view.update(&mise_line("mise node@22.11.0 install"));
        view.update(&mise_line("mise node@22.11.0 download node.tar.gz"));

        assert_eq!(
            spinner_line(&term),
            "Installing mise tools (0s): mise node@22.11.0 download node.tar.gz"
        );
    }

    #[test]
    fn spinner_truncates_the_line_to_the_terminal_width() {
        let (mut view, term) = mise_terminal_view(40);

        view.update(&mise_line(&"x".repeat(100)));

        let contents = term.contents();
        assert_eq!(contents.lines().count(), 1, "{contents}");
        assert!(contents.chars().count() <= 40, "{contents}");
        assert!(
            contents.contains("Installing mise tools (0s): xx"),
            "{contents}"
        );
    }

    #[test]
    fn spinner_is_cleared_when_the_install_completes() {
        let (mut view, term) = mise_terminal_view(120);

        view.update(&mise_line("mise node@22.11.0 install"));
        view.update(&mise_completed());

        assert_eq!(term.contents(), "");
    }

    #[test]
    fn spinner_is_cleared_when_the_install_fails() {
        let (mut view, term) = mise_terminal_view(120);

        view.update(&mise_line("mise node@22.11.0 install"));
        drop(view);

        assert_eq!(term.contents(), "");
    }

    #[test]
    fn spinner_shows_nothing_without_progress() {
        let (mut view, term) = mise_terminal_view(120);

        view.finish();
        drop(view);

        assert_eq!(term.contents(), "");
    }

    #[test]
    fn mise_lines_show_the_start_and_end_of_the_install_only() {
        let messages = [
            mise_line(""),
            mise_line("mise node@22.11.0 install"),
            mise_completed(),
            // Messages after the last one are ignored.
            mise_line("late"),
        ];

        assert_eq!(
            mise_lines(&messages, true),
            "Installing mise tools...\nInstalled mise tools\n"
        );
    }

    #[test]
    fn mise_lines_end_the_install_when_the_sandbox_starts_before_it_completed() {
        assert_eq!(
            mise_lines(&[mise_line("")], true),
            "Installing mise tools...\nInstalled mise tools\n"
        );
    }

    #[test]
    fn mise_lines_show_nothing_without_progress() {
        assert_eq!(mise_lines(&[], true), "");
    }

    #[test]
    fn compact_duration_shows_seconds_minutes_and_hours() {
        assert_eq!(compact_duration(Duration::from_millis(12_900)), "12s");
        assert_eq!(compact_duration(Duration::from_secs(72)), "1m12s");
        assert_eq!(compact_duration(Duration::from_secs(3723)), "1h02m03s");
    }
}
