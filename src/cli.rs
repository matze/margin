//! Command-line surface (PRD §12).
//!
//! With no subcommand `margin` opens the TUI; the subcommands are the headless
//! interface the agent and scripts use.

use clap::{Parser, Subcommand, ValueEnum};

use margin::tui::ThemeMode;
use margin::vcs::Kind;

/// A local TUI for code-review annotations over git/jj.
#[derive(Debug, Parser)]
#[command(name = "margin", version, about)]
pub struct Cli {
    /// Base ref; the sidebar lists commits unique to `<base>..@`.
    #[arg(long, global = true)]
    pub base: Option<String>,

    /// When no base resolves, list this many recent commits.
    #[arg(short = 'n', long = "number", global = true, default_value_t = 50)]
    pub number: usize,

    /// Open straight into a specific commit's diff.
    #[arg(long, global = true)]
    pub rev: Option<String>,

    /// Force a theme instead of detecting it from the terminal.
    #[arg(long, global = true, value_enum)]
    pub theme: Option<ThemeChoice>,

    /// Force a VCS backend instead of auto-detecting (jj preferred, else git).
    #[arg(long, global = true, value_enum)]
    pub vcs: Option<VcsChoice>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Explicit backend override for `--vcs`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum VcsChoice {
    Git,
    Jj,
}

impl From<VcsChoice> for Kind {
    fn from(choice: VcsChoice) -> Self {
        match choice {
            VcsChoice::Git => Kind::Git,
            VcsChoice::Jj => Kind::Jj,
        }
    }
}

/// Explicit theme override for `--theme`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ThemeChoice {
    Light,
    Dark,
}

impl From<ThemeChoice> for ThemeMode {
    fn from(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::Light => ThemeMode::Light,
            ThemeChoice::Dark => ThemeMode::Dark,
        }
    }
}

/// Headless subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print annotations: the agent's read interface.
    List {
        /// Only show annotations whose effective status is open.
        #[arg(long)]
        open: bool,
        /// Emit machine-readable JSON instead of one line per annotation.
        #[arg(long)]
        json: bool,
    },

    /// Change an annotation's state (the agent's write interface).
    Status {
        /// Annotation id or unique id prefix.
        id: String,
        /// Target state: `resolved`, `wont-do`, or `open` (reopen).
        state: AnnotationState,
        /// Reply recorded with a `resolved`/`wont-do` transition.
        #[arg(long)]
        reply: Option<String>,
        /// Reason recorded when reopening (`open`).
        #[arg(long)]
        reason: Option<String>,
        /// Revision that addressed the annotation (for `resolved`); inferred from
        /// the current `HEAD`/`@` when omitted.
        #[arg(long = "addressed-by")]
        addressed_by: Option<String>,
    },

    /// Install the agent skill that teaches a coding agent the `margin` CLI
    /// contract (into `~/.claude/skills/`).
    InstallSkill,
}

/// The state an annotation can be moved to via `margin status`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AnnotationState {
    /// The agent addressed the annotation.
    Resolved,
    /// The agent declined the annotation.
    #[value(name = "wont-do")]
    WontDo,
    /// Reopen a resolved/declined annotation for re-review.
    Open,
}
