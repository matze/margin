//! Command-line surface (PRD §12).
//!
//! With no subcommand `margin` opens the TUI; the subcommands are the headless
//! interface the agent and scripts use.

use clap::{Parser, Subcommand, ValueEnum};

use margin::tui::ThemeMode;

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

    #[command(subcommand)]
    pub command: Option<Command>,
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

    /// Mark an annotation resolved (the agent's write interface).
    Resolve {
        /// Annotation id or unique id prefix.
        id: String,
        /// Optional reply recorded with the resolution.
        #[arg(long)]
        reply: Option<String>,
    },
}
