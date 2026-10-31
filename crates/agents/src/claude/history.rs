//! Rebuilding a Claude thread's history after a restart.
//!
//! Claude Code keeps each session as JSON lines under
//! `~/.claude/projects/<project>/<session id>.jsonl` (or under
//! `$CLAUDE_CONFIG_DIR`). The records carry the same `user` and `assistant`
//! messages as the live stream, so they are replayed through the same
//! [`Mapper`] and come out as the same transcript a live turn produces.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use serde_json::{json, Value};

use super::mapper::{Mapper, Output};
use crate::{events::AgentEvent, protocol::*};

/// Where Claude Code stores session `id`, if it exists.
pub fn find_session_file(id: &str) -> Option<PathBuf> {
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))?;
    find_in(&config.join("projects"), id)
}

fn find_in(projects: &Path, id: &str) -> Option<PathBuf> {
    let name = format!("{id}.jsonl");
    std::fs::read_dir(projects)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(&name))
        .find(|path| path.is_file())
}

/// The turns of a stored session, and the model it last used.
pub fn turns_from_jsonl(jsonl: &str, thread_id: &str, cwd: &str) -> (Vec<Turn>, Option<String>) {
    let mut mapper = Mapper::new(thread_id, cwd, "history");
    let mut turns = Vec::new();
    let mut current: Option<TurnBuilder> = None;

    for line in jsonl.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else { continue };
        if record["isSidechain"].as_bool() == Some(true) || record["isMeta"].as_bool() == Some(true) {
            continue;
        }
        match record["type"].as_str() {
            Some("user") => match prompt_text(&record["message"]["content"]) {
                Some(text) => {
                    if let Some(done) = current.take() {
                        turns.push(done.finish(&mut mapper));
                    }
                    let (turn, events) = mapper.begin_turn(&text);
                    let mut builder = TurnBuilder::new(turn);
                    builder.add_events(events);
                    current = Some(builder);
                }
                None => {
                    if let Some(builder) = &mut current {
                        builder.add(mapper.handle_value(&record));
                    }
                }
            },
            Some("assistant") => {
                if let Some(builder) = &mut current {
                    builder.add(mapper.handle_value(&record));
                }
            }
            _ => {}
        }
    }
    if let Some(done) = current.take() {
        turns.push(done.finish(&mut mapper));
    }
    (turns, mapper.model().map(str::to_string))
}

/// A prompt the user typed, as opposed to tool results or the output of
/// slash commands that Claude Code also records as user messages.
fn prompt_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => {
            if blocks.iter().any(|b| b["type"] == "tool_result") {
                return None;
            }
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => return None,
    };
    let trimmed = text.trim_start();
    let generated = ["<command-", "<local-command", "Caveat:", "[Request interrupted"];
    (!trimmed.is_empty() && !generated.iter().any(|p| trimmed.starts_with(p))).then_some(text)
}

struct TurnBuilder {
    turn: Turn,
    items: Vec<ThreadItem>,
    index: HashMap<String, usize>,
}

impl TurnBuilder {
    fn new(turn: Turn) -> Self {
        Self { turn, items: Vec::new(), index: HashMap::new() }
    }

    fn add(&mut self, outputs: Vec<Output>) {
        self.add_events(outputs.into_iter().filter_map(|o| match o {
            Output::Event(event) => Some(event),
            _ => None,
        }));
    }

    fn add_events(&mut self, events: impl IntoIterator<Item = AgentEvent>) {
        for event in events {
            let item = match event {
                AgentEvent::ItemStarted { item, .. } | AgentEvent::ItemCompleted { item, .. } => item,
                _ => continue,
            };
            let Some(id) = item.id().map(str::to_string) else { continue };
            match self.index.get(&id) {
                Some(&ix) => self.items[ix] = item,
                None => {
                    self.index.insert(id, self.items.len());
                    self.items.push(item);
                }
            }
        }
    }

    fn finish(mut self, mapper: &mut Mapper) -> Turn {
        // Closes tools that never reported a result.
        let end = mapper.handle_value(&json!({"type": "result", "subtype": "success", "is_error": false}));
        self.add(end);
        self.turn.status = TurnStatus::Completed;
        self.turn.items = self.items;
        self.turn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_session_becomes_turns() {
        let jsonl = [
            json!({"type": "summary", "summary": "Fix tests"}),
            json!({"type": "user", "message": {"role": "user", "content": "<command-name>/model</command-name>"}}),
            json!({"type": "user", "message": {"role": "user", "content": "run the tests"}, "uuid": "u1"}),
            json!({"type": "assistant", "message": {"id": "msg_a", "model": "claude-sonnet-5-5", "content": [
                {"type": "text", "text": "Running them."},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "cargo test"}}]}}),
            json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok", "is_error": false}]}}),
            json!({"type": "assistant", "isSidechain": true, "message": {"id": "msg_side", "content": [{"type": "text", "text": "subagent"}]}}),
            json!({"type": "assistant", "message": {"id": "msg_b", "model": "claude-sonnet-5-5", "content": [{"type": "text", "text": "All green."}]}}),
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "now lint"}]}}),
            json!({"type": "assistant", "message": {"id": "msg_c", "model": "claude-sonnet-5-5", "content": [
                {"type": "tool_use", "id": "toolu_2", "name": "Bash", "input": {"command": "cargo clippy"}}]}}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");

        let (turns, model) = turns_from_jsonl(&format!("{jsonl}\nnot json"), "thr", "/w");
        assert_eq!(model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(turns.len(), 2);
        let first: Vec<&ThreadItem> = turns[0].items.iter().collect();
        assert!(matches!(first[0], ThreadItem::UserMessage { .. }));
        assert!(matches!(first[1], ThreadItem::AgentMessage { text, .. } if text == "Running them."));
        assert!(matches!(first[2], ThreadItem::CommandExecution { status: ItemStatus::Completed, .. }));
        assert!(matches!(first[3], ThreadItem::AgentMessage { text, .. } if text == "All green."));
        assert_eq!(first.len(), 4, "subagent output is left out");
        // A tool with no recorded result is closed, not left spinning.
        assert!(matches!(turns[1].items.last(), Some(ThreadItem::CommandExecution { status: ItemStatus::Failed, .. })));
        assert!(turns.iter().all(|t| t.status == TurnStatus::Completed));
        assert_ne!(turns[0].id, turns[1].id);

        // And the transcript renders it like a live thread.
        let transcript = crate::Transcript::from_turns(&turns);
        assert_eq!(transcript.entries().len(), 6);
    }

    #[test]
    fn finds_session_files_in_any_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("-Users-me-src-app");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("abc.jsonl"), "{}").unwrap();
        assert_eq!(find_in(dir.path(), "abc"), Some(project.join("abc.jsonl")));
        assert_eq!(find_in(dir.path(), "missing"), None);
    }
}
