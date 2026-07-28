//! Command-line surface (PRD §12).
//!
//! With no subcommand `margin` opens the TUI; the subcommands are the headless
//! interface the agent and scripts use.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

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

        /// Block until the reviewer hands the review off in the TUI (`H`), then
        /// print. Without it the annotations are printed as they stand.
        #[arg(long)]
        watch: bool,
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
    /// contract (into `~/.claude/skills/` by default).
    InstallSkill {
        /// Write the skill to stdout instead of installing it, to redirect into
        /// whatever file another agent reads (e.g. `>> AGENTS.md`).
        #[arg(long)]
        print: bool,

        /// Install into this skills root instead of `~/.claude/skills`.
        #[arg(long, conflicts_with = "print")]
        dir: Option<PathBuf>,
    },
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
