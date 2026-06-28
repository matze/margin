use serde::{Deserialize, Serialize};

use super::{CommitId, LineNumber, RepoRelPath, RevisionId, Side};

/// The durable reference to where an annotation points (PRD §7, §8).
///
/// Beyond the line range at a given revision, the anchor captures the anchored
/// text and a window of leading/trailing context so the location can be
/// re-found after the file is edited underneath it. Resolution against current
/// code lives in the `anchor` module; this is the stored shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// File the annotation lives in, relative to the repository root.
    pub file: RepoRelPath,
    /// Revision in whose version `start_line`/`end_line` are expressed.
    pub revision_id: RevisionId,
    /// Concrete commit `revision_id` pointed at when the anchor was captured.
    /// Compared against the change's current commit to detect amend/rebase.
    pub commit_at_capture: CommitId,
    /// First anchored line (1-based, inclusive).
    pub start_line: LineNumber,
    /// Last anchored line (1-based, inclusive; equals `start_line` for a single line).
    pub end_line: LineNumber,
    /// Which side of the diff the range refers to.
    pub side: Side,
    /// Lines immediately preceding the range, captured for re-location.
    #[serde(default)]
    pub context_before: Vec<String>,
    /// Lines immediately following the range, captured for re-location.
    #[serde(default)]
    pub context_after: Vec<String>,
    /// The exact text of the anchored range at capture time.
    pub anchored_text: Vec<String>,
}
