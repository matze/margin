//! Machine-readable view of the review for the agent (PRD §10).
//!
//! The NDJSON store is the source of truth; [`render_json`] is the stable,
//! folded projection the agent reads via `margin list --json`.

use serde::Serialize;

use crate::anchor::Resolution;
use crate::model::{AnnotationType, Status};
use crate::review::ResolvedAnnotation;

/// Errors from rendering the JSON view.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("failed to serialize annotations: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Render the stable, machine-readable JSON view: one object per annotation.
pub fn render_json(annotations: &[ResolvedAnnotation]) -> Result<String, ExportError> {
    let view: Vec<AnnotationView> = annotations.iter().map(AnnotationView::from).collect();
    Ok(serde_json::to_string_pretty(&view)?)
}

/// The serialized shape of one annotation in the JSON view.
#[derive(Debug, Serialize)]
struct AnnotationView<'a> {
    id: String,
    file: &'a std::path::Path,
    status: Status,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    annotation_type: Option<AnnotationType>,
    body: &'a str,
    revision_id: &'a str,
    /// Current 1-based location `[start, end]`, or null when orphaned.
    location: Option<[u32; 2]>,
    /// True when the anchor no longer resolves, regardless of `status` — so a
    /// resolved/declined annotation whose lines vanished is still legible.
    orphaned: bool,
    anchored_text: &'a [String],
    addressed_by: Vec<&'a str>,
}

impl<'a> From<&'a ResolvedAnnotation> for AnnotationView<'a> {
    fn from(resolved: &'a ResolvedAnnotation) -> Self {
        let annotation = &resolved.annotation;

        Self {
            id: annotation.id.0.to_string(),
            file: &annotation.anchor.file.0,
            status: resolved.status,
            annotation_type: annotation.annotation_type,
            body: &annotation.body,
            revision_id: &annotation.anchor.revision_id.0,
            location: match resolved.location {
                Resolution::Located { start, end } => Some([start.get(), end.get()]),
                Resolution::Orphaned => None,
            },
            orphaned: matches!(resolved.location, Resolution::Orphaned),
            anchored_text: &annotation.anchor.anchored_text,
            addressed_by: annotation.addressed_by.iter().map(|r| r.0.as_str()).collect(),
        }
    }
}

/// Human-readable status label.
pub fn status_label(status: Status) -> &'static str {
    match status {
        Status::Open => "open",
        Status::Resolved => "resolved",
        Status::WontDo => "wont_do",
        Status::Orphaned => "orphaned",
    }
}

/// Human-readable annotation-type label.
pub fn type_label(annotation_type: AnnotationType) -> &'static str {
    match annotation_type {
        AnnotationType::Fix => "fix",
        AnnotationType::Question => "question",
        AnnotationType::Suggestion => "suggestion",
        AnnotationType::Nit => "nit",
        AnnotationType::Praise => "praise",
    }
}
