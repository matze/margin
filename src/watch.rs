//! Blocking on the reviewer's hand-off, so a waiting agent need not poll.
//!
//! The TUI records a hand-off as a [`EventKind::ReviewerHandedOff`] event per
//! open annotation. [`wait_for_handoff`] watches the log and returns once one of
//! those lands, which is the agent's signal that the review is finished and the
//! annotations are its to act on.

use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc;

use notify::{RecursiveMode, Watcher};

use crate::model::{Event, EventId, EventKind};
use crate::store::{Store, StoreError};

/// Errors from waiting on the annotation log.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("failed to read the annotation store: {0}")]
    Store(#[from] StoreError),
    #[error("failed to watch the annotation store: {0}")]
    Notify(#[from] notify::Error),
    #[error("failed to prepare {path} for watching: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the annotation store is no longer being watched")]
    Disconnected,
}

/// Block until the reviewer hands off, then return.
///
/// Only hand-offs recorded *after* this call are honored: an earlier one belongs
/// to a review that has already been handed to somebody. Returns immediately if
/// one lands between the initial read and the watch being established.
pub fn wait_for_handoff(store: &Store) -> Result<(), WatchError> {
    let known: HashSet<EventId> = store.load()?.iter().map(|event| event.event_id).collect();

    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |_| {
        let _ = sender.send(());
    })?;

    // The log itself may not exist yet, so watch the directory holding it.
    let dir = watched_dir(store)?;
    watcher.watch(dir, RecursiveMode::NonRecursive)?;

    loop {
        if handed_off(&known, &store.load()?) {
            return Ok(());
        }

        receiver.recv().map_err(|_| WatchError::Disconnected)?;
    }
}

/// The store's parent directory, created if it does not exist yet.
fn watched_dir(store: &Store) -> Result<&Path, WatchError> {
    let dir = store.path().parent().unwrap_or_else(|| Path::new("."));

    std::fs::create_dir_all(dir).map_err(|source| WatchError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    Ok(dir)
}

/// Whether `events` contains a hand-off that was not already in `known`.
fn handed_off(known: &HashSet<EventId>, events: &[Event]) -> bool {
    events
        .iter()
        .any(|event| event.kind == EventKind::ReviewerHandedOff && !known.contains(&event.event_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, AnnotationId};

    fn handoff() -> Event {
        Event::now(
            AnnotationId::new(),
            Actor::Reviewer,
            EventKind::ReviewerHandedOff,
        )
    }

    #[test]
    fn a_fresh_handoff_is_detected() {
        let event = handoff();
        assert!(handed_off(&HashSet::new(), &[event]));
    }

    #[test]
    fn a_handoff_from_a_previous_review_is_ignored() {
        let event = handoff();
        let known = HashSet::from([event.event_id]);

        assert!(!handed_off(&known, &[event]));
    }

    #[test]
    fn other_events_do_not_signal_a_handoff() {
        let event = Event::now(
            AnnotationId::new(),
            Actor::Agent,
            EventKind::AgentResolved { reply: None },
        );

        assert!(!handed_off(&HashSet::new(), &[event]));
    }
}
