//! Resolving stored annotations against current code.
//!
//! The store yields each annotation's *derived* state (PRD §10.1); reviewing or
//! exporting also needs each annotation's *current* location and whether its
//! anchor still resolves. This module overlays re-anchoring (PRD §7) onto the
//! folded annotations and produces the view the CLI, exporters, and TUI share.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::anchor::{Resolution, resolve};
use crate::model::{Annotation, AnnotationId, CommitId, Side, Status};
use crate::store::{Store, StoreError};
use crate::vcs::{ChangeCommits, Vcs};

/// How an annotation's anchored change stands in current history, independent of
/// whether its text still resolves. Derived by comparing the change's current
/// commit(s) against the commit captured with the anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionState {
    /// The change still resolves to the commit captured with the anchor.
    Unchanged,
    /// The change resolves to a single, different commit: amended or rebased.
    Amended { current: CommitId },
    /// The change resolves to multiple commits: it is divergent.
    Divergent { commits: Vec<CommitId> },
    /// The change no longer resolves in history: it was abandoned.
    Abandoned,
    /// The backend cannot track change identity across history edits (git).
    Unsupported,
}

impl RevisionState {
    /// Classify a captured commit against the change's current commit(s).
    fn classify(captured: &CommitId, current: &ChangeCommits) -> Self {
        match current {
            ChangeCommits::None => RevisionState::Abandoned,
            ChangeCommits::Many(commits) => RevisionState::Divergent {
                commits: commits.clone(),
            },
            ChangeCommits::Unsupported => RevisionState::Unsupported,
            ChangeCommits::One(commit) if commit == captured => RevisionState::Unchanged,
            ChangeCommits::One(commit) => RevisionState::Amended {
                current: commit.clone(),
            },
        }
    }
}

/// An annotation paired with its location in the current working tree.
#[derive(Debug, Clone)]
pub struct ResolvedAnnotation {
    pub annotation: Annotation,
    /// Where the anchor currently resolves, or that it is orphaned.
    pub location: Resolution,
    /// Effective status: the derived status, downgraded to [`Status::Orphaned`]
    /// when an otherwise-open annotation can no longer be located.
    pub status: Status,
    /// How the anchored change stands in current history (amended, abandoned, …).
    pub revision_state: RevisionState,
}

impl ResolvedAnnotation {
    /// The annotation's id.
    pub fn id(&self) -> AnnotationId {
        self.annotation.id
    }
}

/// Resolve every stored annotation, sorted by file then current (or recorded)
/// start line. New-side anchors resolve against the working tree under
/// `repo_root`; old-side (deleted-line) anchors resolve against the annotated
/// revision's parent via `vcs`.
pub fn resolve_all(
    store: &Store,
    repo_root: impl AsRef<Path>,
    vcs: &dyn Vcs,
) -> Result<Vec<ResolvedAnnotation>, StoreError> {
    let repo_root = repo_root.as_ref();
    let mut cache: BTreeMap<SourceKey, Option<String>> = BTreeMap::new();
    let mut revisions: BTreeMap<String, ChangeCommits> = BTreeMap::new();

    let mut resolved: Vec<ResolvedAnnotation> = store
        .annotations()?
        .into_values()
        .map(|annotation| resolve_one(annotation, repo_root, vcs, &mut cache, &mut revisions))
        .collect();

    resolved.sort_by(|a, b| {
        let key = |r: &ResolvedAnnotation| {
            (
                r.annotation.anchor.file.0.clone(),
                current_start(r).unwrap_or(u32::MAX),
            )
        };
        key(a).cmp(&key(b))
    });

    Ok(resolved)
}

/// The current 1-based start line of an annotation, if located.
pub fn current_start(resolved: &ResolvedAnnotation) -> Option<u32> {
    match resolved.location {
        Resolution::Located { start, .. } => Some(start.get()),
        Resolution::Orphaned => None,
    }
}

/// Identifies the source text an anchor resolves against, so it can be cached:
/// the working tree for new-side anchors, a revision's parent for old-side.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SourceKey {
    WorkingTree(PathBuf),
    Parent { revision: String, path: PathBuf },
}

fn resolve_one(
    annotation: Annotation,
    repo_root: &Path,
    vcs: &dyn Vcs,
    cache: &mut BTreeMap<SourceKey, Option<String>>,
    revisions: &mut BTreeMap<String, ChangeCommits>,
) -> ResolvedAnnotation {
    let anchor = &annotation.anchor;
    let path = &anchor.file.0;

    let key = match anchor.side {
        Side::New => SourceKey::WorkingTree(path.clone()),
        Side::Old => SourceKey::Parent {
            revision: anchor.revision_id.0.clone(),
            path: path.clone(),
        },
    };

    let contents = cache.entry(key).or_insert_with(|| match anchor.side {
        Side::New => std::fs::read_to_string(repo_root.join(path)).ok(),
        Side::Old => vcs.file_at_parent(&anchor.revision_id, &anchor.file).ok(),
    });

    let location = match contents {
        Some(contents) => resolve(&annotation.anchor, contents),
        None => Resolution::Orphaned,
    };

    let status = match (annotation.status, location) {
        (Status::Open, Resolution::Orphaned) => Status::Orphaned,
        (status, _) => status,
    };

    let current = revisions
        .entry(anchor.revision_id.0.clone())
        .or_insert_with(|| {
            // An error querying the change (e.g. a missing backend) is not a
            // history fact, so fall back to "untracked" rather than abandoned.
            vcs.change_commits(&anchor.revision_id)
                .unwrap_or(ChangeCommits::Unsupported)
        });
    let revision_state = RevisionState::classify(&anchor.commit_at_capture, current);

    ResolvedAnnotation {
        annotation,
        location,
        status,
        revision_state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str) -> CommitId {
        CommitId(id.into())
    }

    #[test]
    fn classify_maps_each_change_state() {
        let captured = commit("c0");

        assert_eq!(
            RevisionState::classify(&captured, &ChangeCommits::One(commit("c0"))),
            RevisionState::Unchanged
        );
        assert_eq!(
            RevisionState::classify(&captured, &ChangeCommits::One(commit("c1"))),
            RevisionState::Amended {
                current: commit("c1")
            }
        );
        assert_eq!(
            RevisionState::classify(
                &captured,
                &ChangeCommits::Many(vec![commit("c1"), commit("c2")])
            ),
            RevisionState::Divergent {
                commits: vec![commit("c1"), commit("c2")]
            }
        );
        assert_eq!(
            RevisionState::classify(&captured, &ChangeCommits::None),
            RevisionState::Abandoned
        );
        assert_eq!(
            RevisionState::classify(&captured, &ChangeCommits::Unsupported),
            RevisionState::Unsupported
        );
    }
}
