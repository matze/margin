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
/// drifts from the source of truth.
const SKILL_MD: &str = include_str!("../../.claude/skills/margin-review/SKILL.md");

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

/// Write the embedded skill into `skills_root/margin-review/SKILL.md`, creating
/// the directory as needed and overwriting any prior copy.
pub fn install(skills_root: &Path) -> std::io::Result<Outcome> {
    let dir = skills_root.join(NAME);
    let file = dir.join("SKILL.md");

    let existed = file.exists();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&file, SKILL_MD)?;

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
        assert_eq!(std::fs::read_to_string(created.path()).unwrap(), SKILL_MD);

        let updated = install(root.path()).unwrap();
        assert!(matches!(updated, Outcome::Updated(_)));
        assert_eq!(updated.path(), created.path());
    }

    #[test]
    fn embedded_skill_has_frontmatter() {
        assert!(SKILL_MD.starts_with("---\n"));
        assert!(SKILL_MD.contains("name: margin-review"));
    }
}
