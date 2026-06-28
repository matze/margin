//! Domain model: identifiers, the annotation anchor, the append-only event log,
//! and the fold that derives current annotation state from that log.
//!
//! The types here make invalid states unrepresentable where it is cheap to do
//! so: identifiers are newtypes, the diff side and actor are enums rather than
//! booleans, and an annotation's [`Status`] is *derived* by folding events
//! (see [`fold`]) rather than stored on any single event.

mod anchor;
mod event;
mod fold;

pub use anchor::Anchor;
pub use event::{Event, EventKind};
pub use fold::{Annotation, fold};

use std::num::NonZeroU32;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable id of a revision under review: a git commit SHA or a jj change id.
///
/// This is the *change identity* used for anchoring: under jj it survives
/// amend/rebase, so the same `RevisionId` keeps pointing at a change as its
/// content evolves. Contrast [`CommitId`], the concrete commit a change resolves
/// to at a point in time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevisionId(pub String);

/// A concrete commit hash: a git commit SHA, or a jj commit id (not its change
/// id). Captured alongside an anchor's [`RevisionId`] so re-anchoring can tell
/// whether the change was amended/rebased since — under jj the [`RevisionId`]
/// alone cannot, as it tracks the change across such rewrites.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommitId(pub String);

/// Identity of an annotation, shared across every event that concerns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AnnotationId(pub Uuid);

/// Identity of a single event in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub Uuid);

impl AnnotationId {
    /// Mint a fresh, time-ordered annotation id.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl EventId {
    /// Mint a fresh, time-ordered event id.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for AnnotationId {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

/// A repository-root-relative file path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoRelPath(pub PathBuf);

/// A 1-based line number. Line zero is not representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LineNumber(pub NonZeroU32);

impl LineNumber {
    /// Construct from a 1-based number, returning `None` for zero.
    pub fn new(value: u32) -> Option<Self> {
        NonZeroU32::new(value).map(Self)
    }

    /// The underlying 1-based value.
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

/// Which side of a diff an anchor refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The post-change side; the default for additions and context.
    New,
    /// The pre-change side; used to anchor deleted lines.
    Old,
}

/// Who produced an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// The human reviewing the change.
    Reviewer,
    /// The coding agent acting on the review.
    Agent,
}

/// The optional taxonomy of an annotation (PRD §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationType {
    Fix,
    Question,
    Suggestion,
    Nit,
    Praise,
}

/// Derived state of an annotation, folded from its event timeline plus the
/// outcome of re-anchoring against current code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Awaiting action.
    Open,
    /// The agent addressed it.
    Resolved,
    /// The agent declined it.
    WontDo,
    /// Its anchor can no longer be located in current code (PRD §7).
    Orphaned,
}
