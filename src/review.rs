//! Resolving stored annotations against current code.
//!
//! The store yields each annotation's *derived* state (PRD §10.1); reviewing or
//! exporting also needs each annotation's *current* location and whether its
//! anchor still resolves. This module overlays re-anchoring (PRD §7) onto the
//! folded annotations and produces the view the CLI, exporters, and TUI share.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::anchor::{resolve, Resolution};
use crate::model::{Annotation, AnnotationId, Status};
use crate::store::{Store, StoreError};

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

/// Resolve every stored annotation against the working tree under `repo_root`,
/// sorted by file then current (or recorded) start line.
pub fn resolve_all(
    store: &Store,
    repo_root: impl AsRef<Path>,
) -> Result<Vec<ResolvedAnnotation>, StoreError> {
    let repo_root = repo_root.as_ref();
    let mut cache: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();

    let mut resolved: Vec<ResolvedAnnotation> = store
        .annotations()?
        .into_values()
        .map(|annotation| resolve_one(annotation, repo_root, &mut cache))
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

fn resolve_one(
    annotation: Annotation,
    repo_root: &Path,
    cache: &mut BTreeMap<PathBuf, Option<String>>,
) -> ResolvedAnnotation {
    let path = &annotation.anchor.file.0;
    let contents = cache
        .entry(path.clone())
        .or_insert_with(|| std::fs::read_to_string(repo_root.join(path)).ok());

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
