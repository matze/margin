//! Launch a headless `claude` session to address review annotations and parse
//! its streamed output for live in-TUI feedback.
//!
//! The subprocess runs on a background thread (the TUI is `futures-lite`-based,
//! not tokio): it reads the child's `stream-json` stdout line by line, turns each
//! line into an [`AgentEvent`], and sends it over an `async_channel` the event
//! loop drains. Parsing is a pure function so it can be unit-tested without
//! spawning anything; only [`spawn`] touches the process boundary.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::model::AnnotationId;

/// Whether the session ended cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Error,
}

/// An update streamed from the headless agent session, folded into the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// The session initialized.
    Started,
    /// The agent emitted assistant text.
    Assistant(String),
    /// The agent invoked a tool (e.g. an edit or a `margin status` call).
    ToolUse { name: String, summary: String },
    /// The session ended, with its final result text.
    Finished { outcome: Outcome, summary: String },
    /// The session could not be launched or died unexpectedly.
    Failed(String),
}

/// Which annotations the session should address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentScope {
    /// A single annotation, by id.
    Focused(AnnotationId),
    /// Every open annotation.
    AllOpen,
}

/// The prompt handed to the headless agent for `scope`. Both forms point at the
/// `margin-review` skill, which encodes the read/edit/write-back workflow.
pub fn prompt_for(scope: &AgentScope) -> String {
    match scope {
        AgentScope::AllOpen => "Use the margin-review skill to address all open margin \
             review annotations: read them with `margin list --json --open`, make the code \
             changes, and record each outcome with `margin status`."
            .to_string(),
        AgentScope::Focused(id) => format!(
            "Use the margin-review skill to address the single margin review annotation with \
             id {id}: read it with `margin list --json`, make the code change, and record the \
             outcome with `margin status {id} ...`.",
            id = id.0
        ),
    }
}

/// The agent program: an override via `MARGIN_AGENT_CMD` (for tests and custom
/// setups), otherwise `claude`.
fn program() -> String {
    std::env::var("MARGIN_AGENT_CMD").unwrap_or_else(|_| "claude".to_string())
}

/// Build the headless `claude` invocation for `scope`, rooted at `repo_root`.
///
/// `bypassPermissions` runs the session non-interactively: it edits files and
/// runs `margin status` (a `Bash` call) without prompting, which `acceptEdits`
/// alone cannot — there is no TTY to answer a prompt, so anything less stalls
/// the session. The environment is inherited (not cleared) so `CLAUDE_CONFIG_DIR`
/// and `PATH` reach the child, letting it find the `margin-review` skill.
fn command(repo_root: &Path, scope: &AgentScope) -> Command {
    let mut command = Command::new(program());

    command
        .current_dir(repo_root)
        .arg("-p")
        .arg(prompt_for(scope))
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--permission-mode")
        .arg("bypassPermissions")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    command
}

/// Run the agent on a background thread, streaming each parsed event to
/// `sender`. A terminal [`AgentEvent::Finished`]/[`AgentEvent::Failed`] always
/// lands so the UI can clear its running state.
pub fn spawn(repo_root: PathBuf, scope: AgentScope, sender: async_channel::Sender<AgentEvent>) {
    std::thread::spawn(move || {
        let mut child = match command(&repo_root, &scope).spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send_blocking(AgentEvent::Failed(format!(
                    "could not launch agent: {error}"
                )));
                return;
            }
        };

        let Some(stdout) = child.stdout.take() else {
            let _ = sender.send_blocking(AgentEvent::Failed("agent produced no output".into()));
            return;
        };

        let mut saw_finish = false;

        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            for event in parse_stream_line(&line) {
                saw_finish |= matches!(event, AgentEvent::Finished { .. });
                let _ = sender.send_blocking(event);
            }
        }

        // The stream may end without a `result` line (e.g. the process died);
        // make sure the UI still learns the session is over.
        if !saw_finish {
            let event = match child.wait() {
                Ok(status) if status.success() => AgentEvent::Finished {
                    outcome: Outcome::Ok,
                    summary: "session ended".into(),
                },
                Ok(status) => AgentEvent::Failed(format!("agent exited with {status}")),
                Err(error) => AgentEvent::Failed(format!("agent failed: {error}")),
            };
            let _ = sender.send_blocking(event);
        }
    });
}

/// Parse one `stream-json` line into zero or more events. Unknown shapes,
/// tool-result echoes, and blank lines yield nothing.
pub fn parse_stream_line(line: &str) -> Vec<AgentEvent> {
    let line = line.trim();

    if line.is_empty() {
        return Vec::new();
    }

    let Ok(parsed) = serde_json::from_str::<StreamLine>(line) else {
        return Vec::new();
    };

    match parsed {
        StreamLine::System { subtype } if subtype.as_deref() == Some("init") => {
            vec![AgentEvent::Started]
        }
        StreamLine::System { .. } => Vec::new(),
        StreamLine::Assistant { message } => message
            .content
            .into_iter()
            .filter_map(content_event)
            .collect(),
        StreamLine::Result {
            is_error, result, ..
        } => {
            let outcome = if is_error {
                Outcome::Error
            } else {
                Outcome::Ok
            };
            let summary = result
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| "session ended".into());
            vec![AgentEvent::Finished { outcome, summary }]
        }
        StreamLine::Other => Vec::new(),
    }
}

/// Turn one assistant content block into an event, dropping empty text.
fn content_event(block: ContentBlock) -> Option<AgentEvent> {
    match block {
        ContentBlock::Text { text } if !text.trim().is_empty() => {
            Some(AgentEvent::Assistant(text.trim().to_string()))
        }
        ContentBlock::Text { .. } => None,
        ContentBlock::ToolUse { name, input } => Some(AgentEvent::ToolUse {
            summary: tool_summary(&name, &input),
            name,
        }),
        ContentBlock::Other => None,
    }
}

/// A short, human-readable summary of a tool call for the log/status line.
fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let field = |key: &str| input.get(key).and_then(|value| value.as_str());

    match name {
        "Bash" => field("command").unwrap_or_default().to_string(),
        "Read" | "Edit" | "Write" | "NotebookEdit" => {
            field("file_path").unwrap_or_default().to_string()
        }
        _ => String::new(),
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamLine {
    System {
        #[serde(default)]
        subtype: Option<String>,
    },
    Assistant {
        message: AssistantMessage,
    },
    Result {
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        result: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_system_line_starts_the_session() {
        let line = r#"{"type":"system","subtype":"init","tools":[]}"#;
        assert_eq!(parse_stream_line(line), vec![AgentEvent::Started]);
    }

    #[test]
    fn non_init_system_lines_are_ignored() {
        assert!(parse_stream_line(r#"{"type":"system","subtype":"other"}"#).is_empty());
    }

    #[test]
    fn assistant_text_and_tool_use_split_into_events() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"Fixing the limiter"},
            {"type":"tool_use","name":"Edit","input":{"file_path":"src/limiter.rs"}}
        ]}}"#;

        assert_eq!(
            parse_stream_line(line),
            vec![
                AgentEvent::Assistant("Fixing the limiter".into()),
                AgentEvent::ToolUse {
                    name: "Edit".into(),
                    summary: "src/limiter.rs".into(),
                },
            ]
        );
    }

    #[test]
    fn bash_tool_summarizes_the_command() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Bash","input":{"command":"margin status abc resolved"}}
        ]}}"#;

        assert_eq!(
            parse_stream_line(line),
            vec![AgentEvent::ToolUse {
                name: "Bash".into(),
                summary: "margin status abc resolved".into(),
            }]
        );
    }

    #[test]
    fn empty_assistant_text_is_dropped() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"  "}]}}"#;
        assert!(parse_stream_line(line).is_empty());
    }

    #[test]
    fn result_line_finishes_with_outcome() {
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#;
        assert_eq!(
            parse_stream_line(ok),
            vec![AgentEvent::Finished {
                outcome: Outcome::Ok,
                summary: "done".into(),
            }]
        );

        let err = r#"{"type":"result","is_error":true,"result":"boom"}"#;
        assert_eq!(
            parse_stream_line(err),
            vec![AgentEvent::Finished {
                outcome: Outcome::Error,
                summary: "boom".into(),
            }]
        );
    }

    #[test]
    fn user_and_garbage_lines_are_ignored() {
        assert!(parse_stream_line(r#"{"type":"user","message":{}}"#).is_empty());
        assert!(parse_stream_line("not json").is_empty());
        assert!(parse_stream_line("   ").is_empty());
    }

    #[test]
    fn focused_prompt_carries_the_id() {
        let id = AnnotationId::new();
        let prompt = prompt_for(&AgentScope::Focused(id));
        assert!(prompt.contains(&id.0.to_string()));
    }

    /// End-to-end across the process boundary: a stub `MARGIN_AGENT_CMD` emits
    /// `stream-json`, and [`spawn`] streams the parsed events back. Ignored
    /// because it mutates a process-global env var; run with `--ignored`.
    #[test]
    #[ignore = "sets a global env var; run with --ignored"]
    fn spawn_streams_stub_events() {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("stub.sh");
        std::fs::write(
            &stub,
            "#!/bin/sh\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\"}'\n\
             echo '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}'\n\
             echo '{\"type\":\"result\",\"is_error\":false,\"result\":\"done\"}'\n",
        )
        .unwrap();

        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        // SAFETY: single-threaded test entry; no other test reads this var.
        unsafe { std::env::set_var("MARGIN_AGENT_CMD", &stub) };

        let (tx, rx) = async_channel::unbounded();
        spawn(dir.path().to_path_buf(), AgentScope::AllOpen, tx);

        let mut events = Vec::new();
        while let Ok(event) = rx.recv_blocking() {
            let done = matches!(event, AgentEvent::Finished { .. });
            events.push(event);

            if done {
                break;
            }
        }

        unsafe { std::env::remove_var("MARGIN_AGENT_CMD") };

        assert_eq!(events.first(), Some(&AgentEvent::Started));
        assert!(events.contains(&AgentEvent::Assistant("hi".into())));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Finished {
                outcome: Outcome::Ok,
                ..
            })
        ));
    }
}
