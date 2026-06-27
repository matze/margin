mod cli;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;

use cli::{Cli, Command};
use margin::export::{render_json, status_label, type_label};
use margin::model::{Actor, AnnotationId, Event, EventKind, Status};
use margin::review::{current_start, resolve_all, ResolvedAnnotation};
use margin::store::Store;
use margin::vcs::{Backend, Base, Kind, Vcs};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Some(Command::List { open, json }) => run_list(*open, *json),
        Some(Command::Resolve { id, reply }) => run_resolve(id, reply.clone()),
        Some(Command::InstallSkill) => run_install_skill(),
        None => run_tui(&cli),
    }
}

/// Launch the TUI: discover the repo, list commits per `--base`/`-n`, detect
/// the theme (or honor `--theme`).
fn run_tui(cli: &Cli) -> Result<()> {
    let backend = discover_backend(cli.vcs.map(Into::into))?;

    let base = match &cli.base {
        Some(branch) => Base::Branch(branch.clone()),
        None => Base::Auto { fallback: cli.number },
    };

    margin::tui::run(backend, base, cli.theme.map(Into::into))
}

/// Discover the backend for the current directory, honoring a `--vcs` override.
fn discover_backend(forced: Option<Kind>) -> Result<Backend> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    Backend::discover(&cwd, forced).context("locating a git or jj repository")
}

/// Discover the repository root for the current directory.
fn repo_root() -> Result<PathBuf> {
    Ok(discover_backend(None)?.root().to_path_buf())
}

/// `margin list`: the agent's read interface. `--json` emits the stable folded
/// projection; otherwise one human-readable line per annotation.
fn run_list(open_only: bool, json: bool) -> Result<()> {
    let root = repo_root()?;
    let store = Store::open(&root);

    let shown: Vec<ResolvedAnnotation> = resolve_all(&store, &root)?
        .into_iter()
        .filter(|a| !open_only || a.status == Status::Open)
        .collect();

    if json {
        println!("{}", render_json(&shown)?);
        return Ok(());
    }

    if shown.is_empty() {
        eprintln!("no annotations");
    }

    for resolved in &shown {
        println!("{}", list_line(resolved));
    }

    Ok(())
}

/// One `list` row: short id, location, status, type, first line of body.
fn list_line(resolved: &ResolvedAnnotation) -> String {
    let id = resolved.id().0.to_string();
    let short = &id[..8];
    let file = resolved.annotation.anchor.file.0.display();

    let location = match current_start(resolved) {
        Some(line) => format!("{file}:{line}"),
        None => format!("{file}:?"),
    };

    let kind = resolved
        .annotation
        .annotation_type
        .map(type_label)
        .unwrap_or("note");

    let body = resolved.annotation.body.lines().next().unwrap_or("");

    format!(
        "{short}  {location}  [{}] {kind}  {body}",
        status_label(resolved.status)
    )
}

/// `margin install-skill`: drop the embedded agent skill into the user's
/// `~/.claude/skills/` so any repo's coding agent learns the `margin` contract.
fn run_install_skill() -> Result<()> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let skills_root = PathBuf::from(home).join(".claude").join("skills");

    let outcome = margin::skill::install(&skills_root)
        .with_context(|| format!("installing skill into {}", skills_root.display()))?;

    let verb = match outcome {
        margin::skill::Outcome::Created(_) => "installed",
        margin::skill::Outcome::Updated(_) => "updated",
    };
    println!("{verb} skill at {}", outcome.path().display());

    Ok(())
}

fn run_resolve(id_prefix: &str, reply: Option<String>) -> Result<()> {
    let root = repo_root()?;
    let store = Store::open(&root);

    let id = find_annotation(&store, id_prefix)?;
    store.append(&Event::now(
        id,
        Actor::Agent,
        EventKind::AgentResolved { reply },
    ))?;

    println!("resolved {}", id.0);
    Ok(())
}

/// Resolve an id prefix to exactly one stored annotation.
fn find_annotation(store: &Store, prefix: &str) -> Result<AnnotationId> {
    let needle = prefix.replace('-', "");

    let matches: Vec<AnnotationId> = store
        .annotations()?
        .into_keys()
        .filter(|id| id.0.simple().to_string().starts_with(&needle))
        .collect();

    match matches.as_slice() {
        [id] => Ok(*id),
        [] => bail!("no annotation matches id {prefix:?}"),
        many => bail!("id {prefix:?} is ambiguous ({} matches)", many.len()),
    }
}
