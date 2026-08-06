//! Machine-readable view of the review for the agent (PRD §10).
//!
//! The NDJSON store is the source of truth; [`render_json`] is the stable,
//! folded projection the agent reads via `margin list --json`.

use jiff::Timestamp;
use serde::Serialize;

use crate::anchor::Resolution;
use crate::model::{Actor, AnnotationType, Event, EventKind, Side, Status};
use crate::review::{ResolvedAnnotation, RevisionState};

/// Errors from rendering the JSON view.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("failed to serialize annotations: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Render the stable, machine-readable JSON view: one object per annotation.
///
/// The output is a versioned envelope — the annotations under `annotations`
/// carry the shape documented in `schema/margin-agent-v1.schema.json` (also
/// retrievable via `margin schema`), and `version` lets a reader refuse
/// parsing an unknown future shape instead of guessing.
pub fn render_json(annotations: &[ResolvedAnnotation]) -> Result<String, ExportError> {
    let view: Vec<AnnotationView> = annotations.iter().map(AnnotationView::from).collect();
    Ok(serde_json::to_string_pretty(&ListEnvelope {
        format: "margin-review/list",
        version: crate::schema::VERSION,
        annotations: view,
    })?)
}

/// The version carrier around the `list --json` projection. The shape is
/// versioned so a reader can detect a future breaking change in-band rather
/// than guessing at fields.
#[derive(Debug, Serialize)]
struct ListEnvelope<'a> {
    format: &'a str,
    version: u32,
    annotations: Vec<AnnotationView<'a>>,
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
    /// `divergent`, or `abandoned`. Omitted when the backend cannot place the
    /// change at all (an annotation on git's working copy).
    #[serde(skip_serializing_if = "Option::is_none")]
    revision_state: Option<&'static str>,
    /// The change's current commit when it differs from the captured one
    /// (`revision_state` is `amended`); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    current_commit: Option<&'a str>,
    anchored_text: &'a [String],
    addressed_by: Vec<&'a str>,
    /// The outcomes recorded against the annotation, oldest first, so a reader
    /// sees what was already tried and why it was rejected.
    history: Vec<HistoryEntry<'a>>,
}

/// One turn in an annotation's review conversation: who acted, which outcome
/// they recorded, and what they said about it.
#[derive(Debug, Serialize)]
struct HistoryEntry<'a> {
    at: &'a Timestamp,
    actor: Actor,
    /// `handed_off`, `resolved`, `wont_do`, `reopened`, or `addressed_by`.
    action: &'static str,
    /// The reply or reopen reason recorded with the transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    /// The change linked as addressing the annotation (`addressed_by` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<&'a str>,
}

impl<'a> HistoryEntry<'a> {
    /// Project an event into a conversation turn, or `None` for events that
    /// record no outcome: creation, edits, deletion, restoration.
    fn from_event(event: &'a Event) -> Option<Self> {
        let (action, text, revision) = match &event.kind {
            EventKind::ReviewerHandedOff => ("handed_off", None, None),
            EventKind::AgentResolved { reply } => ("resolved", reply.as_deref(), None),
            EventKind::AgentWontDo { reply } => ("wont_do", reply.as_deref(), None),
            EventKind::ReviewerReopened { reason } => ("reopened", reason.as_deref(), None),

            // A bare link is already carried by `addressed_by`; only a reply
            // recorded alongside it would otherwise be lost.
            EventKind::AgentAddressedBy {
                revision_id,
                reply: Some(reply),
            } => (
                "addressed_by",
                Some(reply.as_str()),
                Some(revision_id.0.as_str()),
            ),

            _ => return None,
        };

        Some(Self {
            at: &event.timestamp,
            actor: event.actor,
            action,
            text,
            revision,
        })
    }
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
            revision_id: annotation.anchor.target.as_str(),
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
            history: annotation
                .timeline
                .iter()
                .filter_map(HistoryEntry::from_event)
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
        Anchor, Annotation, AnnotationId, CommitId, EventId, LineNumber, RepoRelPath, ReviewTarget,
        RevisionId,
    };

    fn resolved(revision_state: RevisionState) -> ResolvedAnnotation {
        ResolvedAnnotation {
            annotation: Annotation {
                id: AnnotationId::new(),
                anchor: Anchor {
                    file: RepoRelPath("f.rs".into()),
                    target: ReviewTarget::Revision(RevisionId("change0".into())),
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

    /// The same annotation carrying a timeline, which `resolved` leaves empty.
    fn with_timeline(timeline: Vec<Event>) -> ResolvedAnnotation {
        ResolvedAnnotation {
            annotation: Annotation {
                timeline,
                ..resolved(RevisionState::Unchanged).annotation
            },
            ..resolved(RevisionState::Unchanged)
        }
    }

    fn event(actor: Actor, secs: i64, kind: EventKind) -> Event {
        Event {
            event_id: EventId::new(),
            annotation_id: AnnotationId::new(),
            timestamp: Timestamp::from_second(secs).unwrap(),
            actor,
            kind,
        }
    }

    /// Parse the single-annotation view back out of the rendered JSON.
    fn history_of(resolved: ResolvedAnnotation) -> serde_json::Value {
        let json = render_json(&[resolved]).unwrap();
        let mut view: serde_json::Value = serde_json::from_str(&json).unwrap();
        view["annotations"][0]["history"].take()
    }

    #[test]
    fn history_carries_replies_and_reopen_reasons_in_order() {
        let history = history_of(with_timeline(vec![
            event(
                Actor::Agent,
                20,
                EventKind::AgentResolved {
                    reply: Some("clamped burst to max".into()),
                },
            ),
            event(
                Actor::Reviewer,
                30,
                EventKind::ReviewerReopened {
                    reason: Some("the default is still 0".into()),
                },
            ),
            event(
                Actor::Agent,
                40,
                EventKind::AgentWontDo {
                    reply: Some("0 is intentional".into()),
                },
            ),
        ]));

        let turns: Vec<(&str, &str, &str)> = history
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| {
                (
                    turn["actor"].as_str().unwrap(),
                    turn["action"].as_str().unwrap(),
                    turn["text"].as_str().unwrap(),
                )
            })
            .collect();

        assert_eq!(
            turns,
            vec![
                ("agent", "resolved", "clamped burst to max"),
                ("reviewer", "reopened", "the default is still 0"),
                ("agent", "wont_do", "0 is intentional"),
            ]
        );
    }

    #[test]
    fn a_transition_without_text_is_still_a_turn() {
        let history = history_of(with_timeline(vec![event(
            Actor::Agent,
            20,
            EventKind::AgentResolved { reply: None },
        )]));

        assert_eq!(history[0]["action"], "resolved");
        assert!(history[0].get("text").is_none(), "{history}");
    }

    #[test]
    fn events_that_record_no_outcome_stay_out_of_history() {
        let history = history_of(with_timeline(vec![
            event(
                Actor::Reviewer,
                10,
                EventKind::AnnotationEdited {
                    body: Some("reworded".into()),
                    annotation_type: None,
                },
            ),
            // Already carried by `addressed_by`, so a bare link adds nothing.
            event(
                Actor::Agent,
                20,
                EventKind::AgentAddressedBy {
                    revision_id: RevisionId("abc".into()),
                    reply: None,
                },
            ),
        ]));

        assert_eq!(history.as_array().unwrap(), &[] as &[serde_json::Value]);
    }

    #[test]
    fn a_reply_on_a_linked_change_is_kept() {
        let history = history_of(with_timeline(vec![event(
            Actor::Agent,
            20,
            EventKind::AgentAddressedBy {
                revision_id: RevisionId("abc".into()),
                reply: Some("split across two commits".into()),
            },
        )]));

        assert_eq!(history[0]["action"], "addressed_by");
        assert_eq!(history[0]["revision"], "abc");
        assert_eq!(history[0]["text"], "split across two commits");
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

    /// The envelope and the field optionality the schema documents: this defends
    /// the contract without a runtime JSON-Schema validator, so a drift in the
    /// shape that `schema/margin-agent-v1.schema.json` pins fails here.
    #[test]
    fn json_view_is_a_versioned_envelope_matching_the_schema() {
        let json = render_json(&[resolved(RevisionState::Amended {
            current: CommitId("commit9".into()),
        })])
        .unwrap();
        let view: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Envelope carrier.
        assert_eq!(view["format"], "margin-review/list");
        assert_eq!(view["version"], crate::schema::VERSION);
        assert_eq!(view["version"], 1);
        assert_eq!(view["annotations"].as_array().unwrap().len(), 1);

        let annotation = &view["annotations"][0];
        // Required keys per the schema.
        for key in [
            "id",
            "file",
            "status",
            "revision_id",
            "side",
            "orphaned",
            "anchored_text",
            "addressed_by",
            "history",
        ] {
            assert!(
                annotation.get(key).is_some(),
                "missing required {key}: {annotation}"
            );
        }
        // `amended` serializes both revision fields.
        assert_eq!(annotation["revision_state"], "amended");
        assert_eq!(annotation["current_commit"], "commit9");
        assert!(annotation.get("type").is_none(), "{annotation}");
    }

    #[test]
    fn unsupported_drops_revision_state_from_the_schema_shape() {
        let json = render_json(&[resolved(RevisionState::Unsupported)]).unwrap();
        let view: serde_json::Value = serde_json::from_str(&json).unwrap();

        let annotation = &view["annotations"][0];
        assert!(annotation.get("revision_state").is_none(), "{annotation}");
        assert!(annotation.get("current_commit").is_none(), "{annotation}");
    }

    /// The schema specifies `location` as `array | null` because an orphaned
    /// anchor serializes `"location": null` (not an absent key) — matching the
    /// SKILL's `location: null` guidance. This locks that the output and schema
    /// agree.
    #[test]
    fn orphaned_serializes_location_as_null() {
        let orphaned = ResolvedAnnotation {
            location: Resolution::Orphaned,
            ..resolved(RevisionState::Unsupported)
        };
        let view: serde_json::Value =
            serde_json::from_str(&render_json(&[orphaned]).unwrap()).unwrap();

        let json = &view["annotations"][0];
        assert_eq!(json["orphaned"], true);
        assert!(json["location"].is_null(), "{json}");
    }

    #[test]
    fn history_revision_is_only_under_addressed_by() {
        let json = render_json(&[with_timeline(vec![
            event(
                Actor::Agent,
                20,
                EventKind::AgentAddressedBy {
                    revision_id: RevisionId("abc".into()),
                    reply: Some("split across two commits".into()),
                },
            ),
            event(
                Actor::Reviewer,
                30,
                EventKind::ReviewerReopened {
                    reason: Some("still 0".into()),
                },
            ),
        ])])
        .unwrap();
        let view: serde_json::Value = serde_json::from_str(&json).unwrap();

        let history = &view["annotations"][0]["history"];
        assert_eq!(history[0]["action"], "addressed_by");
        assert_eq!(history[0]["revision"], "abc");
        assert_eq!(history[1]["action"], "reopened");
        assert!(history[1].get("revision").is_none(), "{history}");
    }
}
