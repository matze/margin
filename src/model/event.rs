use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::{Actor, Anchor, AnnotationId, AnnotationType, EventId, RevisionId};

/// One line of `.margin/annotations.ndjson`: the shared envelope plus exactly
/// one event payload (PRD §8, §10.1).
///
/// The payload is an internally tagged enum keyed by `event`, flattened into
/// the envelope so each event serializes to a single flat JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Identity of this event.
    pub event_id: EventId,
    /// The annotation this event concerns.
    pub annotation_id: AnnotationId,
    /// When the event was recorded.
    pub timestamp: Timestamp,
    /// Who produced it.
    pub actor: Actor,
    /// The event payload.
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    /// Build an event with a freshly minted id and the current timestamp.
    pub fn now(annotation_id: AnnotationId, actor: Actor, kind: EventKind) -> Self {
        Self {
            event_id: EventId::new(),
            annotation_id,
            timestamp: Timestamp::now(),
            actor,
            kind,
        }
    }
}

/// The set of event payloads (PRD §10.1). `status` is never carried here; it is
/// derived by folding the stream (see [`super::fold`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    /// The annotation is born with its anchor, body, and optional type.
    AnnotationCreated {
        anchor: Anchor,
        body: String,
        #[serde(rename = "type", skip_serializing_if = "Option::is_none", default)]
        annotation_type: Option<AnnotationType>,
    },
    /// The reviewer revised the body and/or type. Absent fields are unchanged.
    AnnotationEdited {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        body: Option<String>,
        #[serde(rename = "type", skip_serializing_if = "Option::is_none", default)]
        annotation_type: Option<AnnotationType>,
    },
    /// The agent addressed the annotation, optionally with a reply.
    AgentResolved {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        reply: Option<String>,
    },
    /// The agent declined the annotation, optionally with a reply.
    AgentWontDo {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        reply: Option<String>,
    },
    /// Links the annotation to the change that addressed it (PRD §10.1).
    AgentAddressedBy {
        revision_id: RevisionId,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        reply: Option<String>,
    },
    /// The reviewer rejected the agent's resolution on re-review.
    ReviewerReopened {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
    /// The reviewer deleted the annotation; it folds away as a tombstone.
    AnnotationDeleted {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
    /// The reviewer undid a deletion; the annotation reappears (PRD §10.1).
    AnnotationRestored {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
}
