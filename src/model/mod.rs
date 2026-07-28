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

/// What is under review: a revision in history, or the uncommitted state of the
/// working tree.
///
/// jj snapshots the working copy into the `@` commit, so there it arrives as an
/// ordinary [`ReviewTarget::Revision`]; git has no such commit, and
/// [`ReviewTarget::WorkingCopy`] names the diff against `HEAD` instead.
///
/// Serializes as a plain string so anchors written before the working copy was
/// reviewable still load: a revision as its id, the working copy as
/// [`ReviewTarget::WORKING_COPY`], which no revision id can collide with.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum ReviewTarget {
    Revision(RevisionId),
    WorkingCopy,
}

impl ReviewTarget {
    /// The reserved string standing for the working copy on the wire and in the
    /// `list --json` projection. Parentheses keep it out of every id syntax.
    pub const WORKING_COPY: &'static str = "(working copy)";

    /// The revision this targets, or `None` for the working copy.
    pub fn revision(&self) -> Option<&RevisionId> {
        match self {
            ReviewTarget::Revision(id) => Some(id),
            ReviewTarget::WorkingCopy => None,
        }
    }

    /// How the target reads on the wire and in listings.
    pub fn as_str(&self) -> &str {
        match self {
            ReviewTarget::Revision(id) => &id.0,
            ReviewTarget::WorkingCopy => Self::WORKING_COPY,
        }
    }
}

impl From<ReviewTarget> for String {
    fn from(target: ReviewTarget) -> Self {
        target.as_str().to_string()
    }
}

impl From<String> for ReviewTarget {
    fn from(value: String) -> Self {
        match value.as_str() {
            ReviewTarget::WORKING_COPY => ReviewTarget::WorkingCopy,
            _ => ReviewTarget::Revision(RevisionId(value)),
        }
    }
}

impl std::fmt::Display for ReviewTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod target_tests {
    use super::*;

    #[test]
    fn a_revision_still_serializes_as_a_bare_id() {
        let target = ReviewTarget::Revision(RevisionId("abc123".into()));
        let json = serde_json::to_string(&target).unwrap();

        assert_eq!(json, "\"abc123\"");
        assert_eq!(
            serde_json::from_str::<ReviewTarget>(&json).unwrap(),
            target,
            "anchors written before the working copy was reviewable still load"
        );
    }

    #[test]
    fn the_working_copy_round_trips_through_its_reserved_string() {
        let json = serde_json::to_string(&ReviewTarget::WorkingCopy).unwrap();

        assert_eq!(json, "\"(working copy)\"");
        assert_eq!(
            serde_json::from_str::<ReviewTarget>(&json).unwrap(),
            ReviewTarget::WorkingCopy
        );
    }
}

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
