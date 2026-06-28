use std::collections::BTreeMap;

use super::{Anchor, AnnotationId, AnnotationType, Event, EventKind, RevisionId, Status};

/// An annotation's current state, derived by folding its event timeline.
///
/// [`Status::Orphaned`] is never produced here — it depends on re-anchoring
/// against current code, which the `anchor` module overlays afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub id: AnnotationId,
    pub anchor: Anchor,
    pub body: String,
    pub annotation_type: Option<AnnotationType>,
    pub status: Status,
    /// The change(s) the agent linked as addressing this annotation, in order.
    pub addressed_by: Vec<RevisionId>,
    /// Every event for this annotation, oldest first.
    pub timeline: Vec<Event>,
}

/// Fold an event stream into current per-annotation state, keyed and ordered by
/// [`AnnotationId`] (PRD §8, §10.1).
///
/// Events are grouped by annotation and replayed in timestamp order. An
/// annotation with no `annotation_created` event is skipped (it cannot be
/// reconstructed); such orphaned events are ignored rather than fatal.
pub fn fold(events: impl IntoIterator<Item = Event>) -> BTreeMap<AnnotationId, Annotation> {
    let mut grouped: BTreeMap<AnnotationId, Vec<Event>> = BTreeMap::new();

    for event in events {
        grouped.entry(event.annotation_id).or_default().push(event);
    }

    grouped
        .into_iter()
        .filter_map(|(id, mut timeline)| {
            timeline.sort_by_key(|event| event.timestamp);
            fold_one(id, timeline).map(|annotation| (id, annotation))
        })
        .collect()
}

fn fold_one(id: AnnotationId, timeline: Vec<Event>) -> Option<Annotation> {
    let mut iter = timeline.iter();

    let (anchor, mut body, mut annotation_type) = iter.find_map(|event| match &event.kind {
        EventKind::AnnotationCreated {
            anchor,
            body,
            annotation_type,
        } => Some((anchor.clone(), body.clone(), *annotation_type)),
        _ => None,
    })?;

    let mut status = Status::Open;
    let mut addressed_by = Vec::new();
    let mut deleted = false;

    for event in &timeline {
        match &event.kind {
            EventKind::AnnotationCreated { .. } => {}

            EventKind::AnnotationDeleted { .. } => deleted = true,

            EventKind::AnnotationRestored { .. } => deleted = false,

            EventKind::AnnotationEdited {
                body: new_body,
                annotation_type: new_type,
            } => {
                if let Some(new_body) = new_body {
                    body = new_body.clone();
                }

                if let Some(new_type) = new_type {
                    annotation_type = Some(*new_type);
                }
            }

            EventKind::AgentResolved { .. } => status = Status::Resolved,
            EventKind::AgentWontDo { .. } => status = Status::WontDo,
            EventKind::ReviewerReopened { .. } => status = Status::Open,

            EventKind::AgentAddressedBy { revision_id, .. } => {
                addressed_by.push(revision_id.clone())
            }
        }
    }

    if deleted {
        return None;
    }

    Some(Annotation {
        id,
        anchor,
        body,
        annotation_type,
        status,
        addressed_by,
        timeline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, CommitId, EventId, LineNumber, RepoRelPath, RevisionId, Side};
    use jiff::Timestamp;
    use std::path::PathBuf;

    fn anchor() -> Anchor {
        Anchor {
            file: RepoRelPath(PathBuf::from("src/lib.rs")),
            revision_id: RevisionId("rev0".into()),
            commit_at_capture: CommitId("commit0".into()),
            start_line: LineNumber::new(12).unwrap(),
            end_line: LineNumber::new(12).unwrap(),
            side: Side::New,
            context_before: vec![],
            context_after: vec![],
            anchored_text: vec!["let x = 1;".into()],
        }
    }

    fn event(id: AnnotationId, secs: i64, kind: EventKind) -> Event {
        Event {
            event_id: EventId::new(),
            annotation_id: id,
            timestamp: Timestamp::from_second(secs).unwrap(),
            actor: Actor::Reviewer,
            kind,
        }
    }

    #[test]
    fn created_edited_resolved_reopened_folds_to_open() {
        let id = AnnotationId::new();
        let events = vec![
            event(
                id,
                10,
                EventKind::AnnotationCreated {
                    anchor: anchor(),
                    body: "burst should be optional".into(),
                    annotation_type: Some(AnnotationType::Fix),
                },
            ),
            event(
                id,
                20,
                EventKind::AnnotationEdited {
                    body: Some("burst should default to max".into()),
                    annotation_type: None,
                },
            ),
            event(id, 30, EventKind::AgentResolved { reply: None }),
            event(
                id,
                40,
                EventKind::ReviewerReopened {
                    reason: Some("default should be 0".into()),
                },
            ),
        ];

        let folded = fold(events);
        let annotation = &folded[&id];

        assert_eq!(annotation.status, Status::Open);
        assert_eq!(annotation.body, "burst should default to max");
        assert_eq!(annotation.annotation_type, Some(AnnotationType::Fix));
        assert_eq!(annotation.timeline.len(), 4);
    }

    #[test]
    fn out_of_order_events_are_sorted_before_folding() {
        let id = AnnotationId::new();
        let events = vec![
            event(id, 30, EventKind::AgentResolved { reply: None }),
            event(
                id,
                10,
                EventKind::AnnotationCreated {
                    anchor: anchor(),
                    body: "note".into(),
                    annotation_type: None,
                },
            ),
        ];

        let folded = fold(events);
        assert_eq!(folded[&id].status, Status::Resolved);
    }

    #[test]
    fn addressed_by_is_collected_in_order() {
        let id = AnnotationId::new();
        let events = vec![
            event(
                id,
                10,
                EventKind::AnnotationCreated {
                    anchor: anchor(),
                    body: "note".into(),
                    annotation_type: None,
                },
            ),
            event(
                id,
                20,
                EventKind::AgentAddressedBy {
                    revision_id: RevisionId("abc".into()),
                    reply: None,
                },
            ),
        ];

        let folded = fold(events);
        assert_eq!(folded[&id].addressed_by, vec![RevisionId("abc".into())]);
    }

    #[test]
    fn events_without_creation_are_skipped() {
        let id = AnnotationId::new();
        let events = vec![event(id, 10, EventKind::AgentResolved { reply: None })];
        assert!(fold(events).is_empty());
    }

    #[test]
    fn a_deleted_annotation_folds_away() {
        let id = AnnotationId::new();
        let events = vec![
            event(
                id,
                10,
                EventKind::AnnotationCreated {
                    anchor: anchor(),
                    body: "note".into(),
                    annotation_type: None,
                },
            ),
            event(id, 20, EventKind::AnnotationDeleted { reason: None }),
        ];

        assert!(fold(events).is_empty());
    }

    #[test]
    fn a_restored_annotation_comes_back() {
        let id = AnnotationId::new();
        let events = vec![
            event(
                id,
                10,
                EventKind::AnnotationCreated {
                    anchor: anchor(),
                    body: "note".into(),
                    annotation_type: None,
                },
            ),
            event(id, 20, EventKind::AnnotationDeleted { reason: None }),
            event(id, 30, EventKind::AnnotationRestored { reason: None }),
        ];

        let folded = fold(events);
        assert_eq!(folded[&id].status, Status::Open);
    }
}
