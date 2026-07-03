//! `margin` — a local TUI for code-review annotations over git/jj.
//!
//! The crate is split into a reusable core (this library) and a thin binary
//! (`src/main.rs`) that wires the CLI and TUI on top. The core is terminal-free
//! so it can be exercised entirely with unit and fixture tests.

pub mod anchor;
pub mod export;
pub mod model;
pub mod review;
pub mod skill;
pub mod store;
pub mod tui;
pub mod vcs;
