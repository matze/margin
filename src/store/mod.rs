//! The append-only event log backing every annotation (PRD §9, §10.1).
//!
//! The store is a single newline-delimited JSON file,
//! `.margin/annotations.ndjson`, at the repository root. It is the source of
//! truth: `margin` and the agent only ever *append* events, so there is no
//! read-modify-write race. Reads fold the full stream into current state.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::model::{fold, Annotation, AnnotationId, Event};

/// Errors from reading or appending to the event log.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("failed to access annotation store at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize event: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Handle to the on-disk event log for one repository.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// The store directory and file names relative to the repository root.
    const DIR: &'static str = ".margin";
    const FILE: &'static str = "annotations.ndjson";

    /// Open the store rooted at `repo_root`. No filesystem access happens until
    /// the first append or load.
    pub fn open(repo_root: impl AsRef<Path>) -> Self {
        let path = repo_root.as_ref().join(Self::DIR).join(Self::FILE);
        Self { path }
    }

    /// Path of the underlying NDJSON file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event as a single JSON line, creating `.margin/` on demand.
    ///
    /// The write is a single `write_all` to a file opened in append mode, which
    /// is atomic for line-sized payloads on local filesystems — concurrent
    /// appends from `margin` and the agent do not corrupt each other.
    pub fn append(&self, event: &Event) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| self.io_err(source))?;
        }

        let mut line = serde_json::to_string(event)?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| self.io_err(source))?;

        file.write_all(line.as_bytes())
            .map_err(|source| self.io_err(source))
    }

    /// Read every event in log order. A missing file yields an empty stream;
    /// malformed lines are reported to stderr and skipped rather than aborting,
    /// so one bad line never hides the rest of the history.
    pub fn load(&self) -> Result<Vec<Event>, StoreError> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(self.io_err(source)),
        };

        let events = contents
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .filter_map(|(index, line)| match serde_json::from_str::<Event>(line) {
                Ok(event) => Some(event),
                Err(error) => {
                    eprintln!(
                        "margin: skipping malformed event at {}:{}: {error}",
                        self.path.display(),
                        index + 1
                    );
                    None
                }
            })
            .collect();

        Ok(events)
    }

    /// Load and fold the log into current per-annotation state.
    pub fn annotations(&self) -> Result<BTreeMap<AnnotationId, Annotation>, StoreError> {
        Ok(fold(self.load()?))
    }

    fn io_err(&self, source: std::io::Error) -> StoreError {
        StoreError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Actor, Anchor, AnnotationId, AnnotationType, Event, EventKind, LineNumber, RepoRelPath,
        RevisionId, Side, Status,
    };
    use std::path::PathBuf;

    fn created_event(id: AnnotationId) -> Event {
        Event::now(
            id,
            Actor::Reviewer,
            EventKind::AnnotationCreated {
                anchor: Anchor {
                    file: RepoRelPath(PathBuf::from("src/limiter.rs")),
                    revision_id: RevisionId("a1b2c3".into()),
                    start_line: LineNumber::new(12).unwrap(),
                    end_line: LineNumber::new(14).unwrap(),
                    side: Side::New,
                    context_before: vec!["pub struct Limiter {".into()],
                    context_after: vec!["}".into()],
                    anchored_text: vec!["window: Duration,".into(), "burst: u32,".into()],
                },
                body: "burst should be optional".into(),
                annotation_type: Some(AnnotationType::Fix),
            },
        )
    }

    #[test]
    fn append_then_load_round_trips_every_variant() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let id = AnnotationId::new();

        let events = vec![
            created_event(id),
            Event::now(
                id,
                Actor::Reviewer,
                EventKind::AnnotationEdited {
                    body: Some("clarified".into()),
                    annotation_type: None,
                },
            ),
            Event::now(
                id,
                Actor::Agent,
                EventKind::AgentAddressedBy {
                    revision_id: RevisionId("5e6f7a".into()),
                    reply: Some("made burst Option<u32>".into()),
                },
            ),
            Event::now(id, Actor::Agent, EventKind::AgentResolved { reply: None }),
            Event::now(id, Actor::Agent, EventKind::AgentWontDo { reply: None }),
            Event::now(
                id,
                Actor::Reviewer,
                EventKind::ReviewerReopened { reason: None },
            ),
        ];

        for event in &events {
            store.append(event).unwrap();
        }

        assert_eq!(store.load().unwrap(), events);
    }

    #[test]
    fn append_only_preserves_prior_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let id = AnnotationId::new();

        store.append(&created_event(id)).unwrap();
        store
            .append(&Event::now(
                id,
                Actor::Agent,
                EventKind::AgentResolved { reply: None },
            ))
            .unwrap();

        let annotations = store.annotations().unwrap();
        assert_eq!(annotations[&id].status, Status::Resolved);
        assert_eq!(annotations[&id].timeline.len(), 2);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let id = AnnotationId::new();

        store.append(&created_event(id)).unwrap();
        fs::write(
            store.path(),
            format!(
                "{}\nnot json\n",
                serde_json::to_string(&created_event(id)).unwrap()
            ),
        )
        .unwrap();

        assert_eq!(store.load().unwrap().len(), 1);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        assert!(store.load().unwrap().is_empty());
    }
}
