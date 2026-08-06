//! The `list --json` output contract: its version and its canonical JSON
//! Schema.
//!
//! `margin schema` prints [`SCHEMA`]; the embedded margin-review skill and the
//! conformance test in `src/export/mod.rs` both key off it. The schema is the
//! committed source of truth for the projection's field/value domains, and
//! [`VERSION`] is the single source of truth for the envelope's `version` — a
//! future breaking format change bumps it and adds `margin-agent-v2.schema.json`
//! rather than overwriting v1.

/// The envelope version of the `list --json` projection. Referenced by the
/// envelope struct in `src/export/mod.rs` so it cannot drift.
pub const VERSION: u32 = 1;

/// The canonical JSON Schema of the `list --json` output, embedded from the
/// repository so it never drifts from the committed artifact.
pub const SCHEMA: &str = include_str!("../../schema/margin-agent-v1.schema.json");
