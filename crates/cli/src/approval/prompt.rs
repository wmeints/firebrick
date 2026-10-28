//! Asks the user on the terminal whether to apply the changes.

use super::Settings;
use super::diff::{difference, listing};
use anyhow::{Context, Result, bail};
use std::io::{self, BufRead, IsTerminal, Write};

/// Error for an answer other than yes.
pub(super) const ABORTED: &str =
    "aborted: the changes to mounts, network and clipboard in .firebrick.yml weren't approved";

/// Error when there is no terminal to ask on and `--yes` isn't passed.
pub(super) const NOT_INTERACTIVE: &str = ".firebrick.yml changes mounts, network or clipboard since you \
                               last approved them; run the command in a terminal to review the \
                               changes, or pass --yes";

/// Header above the changes to approve.
const CHANGES_HEADER: &str =
    ".firebrick.yml changes settings you haven't approved for this workspace:";

/// Header above the settings of a workspace without approved settings.
const FIRST_HEADER: &str = ".firebrick.yml sets mounts, network or clipboard settings you haven't \
                            approved for this workspace yet:";

/// How a prompt gets its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Approve the changes without asking, for `--yes`.
    Yes,
    /// Ask the user.
    Ask,
    /// Nobody can answer, because stdin isn't a terminal.
    NoTerminal,
}

/// Asks the user to approve changes.
pub struct Prompt<R, W> {
    answer: Answer,
    input: R,
    pub(super) output: W,
}

impl Prompt<io::StdinLock<'static>, io::Stderr> {
    /// Returns a prompt that asks on the terminal, or approves without asking with
    /// `assume_yes`.
    pub fn terminal(assume_yes: bool) -> Self {
        let stdin = io::stdin();
        let answer = match (assume_yes, stdin.is_terminal()) {
            (true, _) => Answer::Yes,
            (false, true) => Answer::Ask,
            (false, false) => Answer::NoTerminal,
        };

        Self::new(answer, stdin.lock(), io::stderr())
    }
}

impl<R: BufRead, W: Write> Prompt<R, W> {
    /// Returns a prompt that reads the answer from `input` and writes to `output`.
    pub fn new(answer: Answer, input: R, output: W) -> Self {
        Self {
            answer,
            input,
            output,
        }
    }

    /// Asks about the difference between the approved and requested settings when the
    /// requested ones differ from the expected ones. Settings equal to the approved ones never
    /// need asking. Without approved settings, asks about all requested settings unless they're
    /// the defaults.
    pub(super) fn review(
        &mut self,
        approved: Option<&Settings>,
        expected: Option<&Settings>,
        requested: &Settings,
    ) -> Result<()> {
        let Some(approved) = approved else {
            return self.review_first(requested);
        };

        let changes = difference(approved, requested);
        let expected = expected.unwrap_or(approved);

        if changes.is_empty() || difference(expected, requested).is_empty() {
            return Ok(());
        }

        self.confirm(CHANGES_HEADER, &changes)
    }

    /// Asks about all requested settings of a workspace without approved ones, unless they're
    /// the defaults.
    fn review_first(&mut self, requested: &Settings) -> Result<()> {
        if difference(&Settings::default(), requested).is_empty() {
            return Ok(());
        }

        self.confirm(FIRST_HEADER, &listing(requested))
    }

    /// Prints the lines under the header and asks to apply them, unless the answer is `Yes`.
    /// Fails when the user doesn't answer yes, or nobody can answer.
    fn confirm(&mut self, header: &str, lines: &[String]) -> Result<()> {
        self.show(header, lines)
            .context("failed to show the settings to approve")?;

        match self.answer {
            Answer::Yes => return Ok(()),
            Answer::NoTerminal => bail!(NOT_INTERACTIVE),
            Answer::Ask => {}
        }

        let answer = self
            .ask()
            .context("failed to read the answer to the approval prompt")?;

        match answer.trim().to_lowercase().as_str() {
            "y" | "yes" => Ok(()),
            _ => bail!(ABORTED),
        }
    }

    /// Writes the header and an indented line per setting.
    fn show(&mut self, header: &str, lines: &[String]) -> io::Result<()> {
        writeln!(self.output, "{header}")?;
        for line in lines {
            writeln!(self.output, "  {line}")?;
        }

        Ok(())
    }

    /// Asks to apply the changes and returns the answer.
    fn ask(&mut self) -> io::Result<String> {
        write!(self.output, "Apply these changes? [y/N] ")?;
        self.output.flush()?;

        let mut answer = String::new();
        self.input.read_line(&mut answer)?;

        Ok(answer)
    }
}
