//! Resolving stored annotations against current code.
//!
//! The store yields each annotation's *derived* state (PRD §10.1); reviewing or
//! exporting also needs each annotation's *current* location and whether its
//! anchor still resolves. This module overlays re-anchoring (PRD §7) onto the
//! folded annotations and produces the view the CLI, exporters, and TUI share.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::anchor::{resolve, Resolution};
use crate::model::{Annotation, AnnotationId, Side, Status};
use crate::store::{Store, StoreError};
use crate::vcs::Vcs;

/// An annotation paired with its location in the current working tree.
#[derive(Debug, Clone)]
pub struct ResolvedAnnotation {
    pub annotation: Annotation,
    /// Where the anchor currently resolves, or that it is orphaned.
    pub location: Resolution,
    /// Effective status: the derived status, downgraded to [`Status::Orphaned`]
    /// when an otherwise-open annotation can no longer be located.
    pub status: Status,
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

    let mut resolved: Vec<ResolvedAnnotation> = store
        .annotations()?
        .into_values()
        .map(|annotation| resolve_one(annotation, repo_root, vcs, &mut cache))
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

    ResolvedAnnotation {
        annotation,
        location,
        status,
    }
}
