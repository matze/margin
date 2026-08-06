//! Installing the agent skill that documents the `margin` CLI contract.
//!
//! The skill (`SKILL.md`) teaches a coding agent how to read annotations via
//! `margin list --json` and write back via `margin status`. It is embedded in
//! the binary so `margin install-skill` can drop it into the user's skills
//! directory regardless of the working directory.

use std::path::{Path, PathBuf};

/// Skill directory name under the skills root.
pub const NAME: &str = "margin-review";

/// The skill document, embedded from the repository so the installed copy never
/// drifts from the source of truth. Public so it can also be written somewhere
/// margin does not know the convention for, such as an `AGENTS.md`.
pub const DOCUMENT: &str = include_str!("../../.claude/skills/margin-review/SKILL.md");

/// The JSON Schema of the `list --json` output, embedded from the repository so
/// the copy `install` drops next to `SKILL.md` never drifts from the committed
/// artifact. Also retrievable via `margin schema`.
pub const SCHEMA: &str = include_str!("../../schema/margin-agent-v1.schema.json");

/// Whether [`install`] created a new skill or overwrote an existing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Created(PathBuf),
    Updated(PathBuf),
}

impl Outcome {
    /// The written `SKILL.md` path.
    pub fn path(&self) -> &Path {
        match self {
            Outcome::Created(path) | Outcome::Updated(path) => path,
        }
    }
}

/// Write the embedded skill and its JSON Schema into
/// `skills_root/margin-review/`, creating the directory as needed and
/// overwriting any prior copy. The schema lands beside `SKILL.md` so an
/// installed skill's reader has the `list --json` contract locally without
/// running `margin schema`.
pub fn install(skills_root: &Path) -> std::io::Result<Outcome> {
    let dir = skills_root.join(NAME);
    let file = dir.join("SKILL.md");

    let existed = file.exists();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&file, DOCUMENT)?;
    std::fs::write(dir.join("schema.json"), SCHEMA)?;

    Ok(if existed {
        Outcome::Updated(file)
    } else {
        Outcome::Created(file)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_creates_then_updates() {
        let root = tempfile::tempdir().unwrap();

        let created = install(root.path()).unwrap();
        assert!(matches!(created, Outcome::Created(_)));
        assert_eq!(std::fs::read_to_string(created.path()).unwrap(), DOCUMENT);
        assert_eq!(
            std::fs::read_to_string(root.path().join(NAME).join("schema.json")).unwrap(),
            SCHEMA
        );

        let updated = install(root.path()).unwrap();
        assert!(matches!(updated, Outcome::Updated(_)));
        assert_eq!(updated.path(), created.path());
    }

    #[test]
    fn embedded_skill_has_frontmatter() {
        assert!(DOCUMENT.starts_with("---\n"));
        assert!(DOCUMENT.contains("name: margin-review"));
    }
}
