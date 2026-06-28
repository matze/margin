//! Machine-readable view of the review for the agent (PRD §10).
//!
//! The NDJSON store is the source of truth; [`render_json`] is the stable,
//! folded projection the agent reads via `margin list --json`.

use serde::Serialize;

use crate::anchor::Resolution;
use crate::model::{AnnotationType, Side, Status};
use crate::review::{ResolvedAnnotation, RevisionState};

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
    /// Which diff side the anchor lives on. For `old`, the annotation marks a
    /// deleted line and `location` refers to the revision's parent.
    side: Side,
    /// Current 1-based location `[start, end]`, or null when orphaned.
    location: Option<[u32; 2]>,
    /// True when the anchor no longer resolves, regardless of `status` — so a
    /// resolved/declined annotation whose lines vanished is still legible.
    orphaned: bool,
    /// How the anchored change stands in history: `unchanged`, `amended`,
    /// `divergent`, or `abandoned`. Omitted on backends without change identity
    /// (git), so its presence also signals jj change tracking is in effect.
    #[serde(skip_serializing_if = "Option::is_none")]
    revision_state: Option<&'static str>,
    /// The change's current commit when it differs from the captured one
    /// (`revision_state` is `amended`); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    current_commit: Option<&'a str>,
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
            side: annotation.anchor.side,
            location: match resolved.location {
                Resolution::Located { start, end } => Some([start.get(), end.get()]),
                Resolution::Orphaned => None,
            },
            orphaned: matches!(resolved.location, Resolution::Orphaned),
            revision_state: revision_state_label(&resolved.revision_state),
            current_commit: match &resolved.revision_state {
                RevisionState::Amended { current } => Some(current.0.as_str()),
                _ => None,
            },
            anchored_text: &annotation.anchor.anchored_text,
            addressed_by: annotation
                .addressed_by
                .iter()
                .map(|r| r.0.as_str())
                .collect(),
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

/// Stable label for an annotation's revision state, or `None` when the backend
/// cannot track change identity (git).
fn revision_state_label(state: &RevisionState) -> Option<&'static str> {
    match state {
        RevisionState::Unchanged => Some("unchanged"),
        RevisionState::Amended { .. } => Some("amended"),
        RevisionState::Divergent { .. } => Some("divergent"),
        RevisionState::Abandoned => Some("abandoned"),
        RevisionState::Unsupported => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::Resolution;
    use crate::model::{
        Anchor, Annotation, AnnotationId, CommitId, LineNumber, RepoRelPath, RevisionId,
    };

    fn resolved(revision_state: RevisionState) -> ResolvedAnnotation {
        ResolvedAnnotation {
            annotation: Annotation {
                id: AnnotationId::new(),
                anchor: Anchor {
                    file: RepoRelPath("f.rs".into()),
                    revision_id: RevisionId("change0".into()),
                    commit_at_capture: CommitId("commit0".into()),
                    start_line: LineNumber::new(1).unwrap(),
                    end_line: LineNumber::new(1).unwrap(),
                    side: Side::New,
                    context_before: vec![],
                    context_after: vec![],
                    anchored_text: vec!["fn f() {}".into()],
                },
                body: "look".into(),
                annotation_type: None,
                status: Status::Open,
                addressed_by: vec![],
                timeline: vec![],
            },
            location: Resolution::Located {
                start: LineNumber::new(1).unwrap(),
                end: LineNumber::new(1).unwrap(),
            },
            status: Status::Open,
            revision_state,
        }
    }

    #[test]
    fn amended_serializes_state_and_current_commit() {
        let json = render_json(&[resolved(RevisionState::Amended {
            current: CommitId("commit9".into()),
        })])
        .unwrap();

        assert!(json.contains("\"revision_state\": \"amended\""), "{json}");
        assert!(json.contains("\"current_commit\": \"commit9\""), "{json}");
    }

    #[test]
    fn unsupported_omits_revision_fields() {
        let json = render_json(&[resolved(RevisionState::Unsupported)]).unwrap();

        assert!(!json.contains("revision_state"), "{json}");
        assert!(!json.contains("current_commit"), "{json}");
    }
}
