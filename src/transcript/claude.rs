use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use super::{StopKind, TurnScan};

/// Pure incremental turn parser for Claude Code transcript JSONL entries.
#[derive(Debug, Default)]
pub struct ClaudeTurnParser {
    started: bool,
    aborted: Option<String>,
    upstream_error: Option<String>,
    switch_to: Option<String>,
    turn_duration: bool,
    current_message_id: Option<String>,
    current_texts: Vec<String>,
    turn_texts: Vec<String>,
    last_stop_reason: Option<String>,
    tool_result_after_last_assistant: bool,
}

impl ClaudeTurnParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, entry: &Value) {
        if entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        match entry.get("type").and_then(Value::as_str).unwrap_or("") {
            "user" => self.feed_user(entry),
            "assistant" => self.feed_assistant(entry),
            "system" => {
                if entry.get("subtype").and_then(Value::as_str) == Some("turn_duration")
                    && entry.get("isMeta").and_then(Value::as_bool) != Some(true)
                {
                    self.turn_duration = true;
                }
            }
            "continued-in" => {
                if let Some(id) = entry.get("continuedInSessionId").and_then(Value::as_str) {
                    self.switch_to = Some(id.to_string());
                }
            }
            _ => {}
        }
    }

    pub fn scan(&self, agent_idle: bool) -> TurnScan {
        if let Some(session_id) = &self.switch_to {
            return TurnScan::SwitchFile {
                session_id: session_id.clone(),
            };
        }
        if let Some(reason) = &self.aborted {
            return TurnScan::Aborted {
                reason: reason.clone(),
            };
        }
        if let Some(message) = &self.upstream_error {
            return TurnScan::UpstreamError {
                message: message.clone(),
            };
        }
        if !self.started {
            return TurnScan::NotStarted;
        }
        let stop = match self.last_stop_reason.as_deref() {
            Some("end_turn") => StopKind::EndTurn,
            Some("max_tokens") => StopKind::MaxTokens,
            Some(other) => StopKind::Other(other.to_string()),
            None => StopKind::Other("unknown".to_string()),
        };
        if self.turn_duration {
            return TurnScan::Completed {
                text: self.final_text(),
                stop,
            };
        }
        let terminal = matches!(
            self.last_stop_reason.as_deref(),
            Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal")
        );
        if agent_idle && terminal && !self.tool_result_after_last_assistant {
            return TurnScan::Completed {
                text: self.final_text(),
                stop,
            };
        }
        TurnScan::InProgress
    }

    fn feed_user(&mut self, entry: &Value) {
        if entry.get("isMeta").and_then(Value::as_bool) == Some(true) {
            return;
        }
        match entry.get("message").and_then(|m| m.get("content")) {
            Some(Value::String(text)) => {
                self.reset_turn();
                self.started = true;
                if text.starts_with("[Request interrupted by user") {
                    self.aborted = Some(text.clone());
                }
            }
            Some(Value::Array(blocks)) => {
                if has_block(blocks, "text") {
                    self.reset_turn();
                    self.started = true;
                    if let Some(text) = interrupted_text(blocks) {
                        self.aborted = Some(text);
                    }
                } else if has_block(blocks, "tool_result") && self.started {
                    self.tool_result_after_last_assistant = true;
                }
            }
            _ => {}
        }
    }

    fn feed_assistant(&mut self, entry: &Value) {
        if !self.started {
            return;
        }
        if entry.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
            let text = content_blocks(entry)
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            self.upstream_error = Some(text);
            return;
        }
        let message = entry.get("message");
        let id = message
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .map(String::from);
        if id != self.current_message_id {
            self.current_message_id = id;
            self.current_texts.clear();
        }
        if let Some(block) = content_blocks(entry).first() {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    self.current_texts.push(text.to_string());
                    self.turn_texts.push(text.to_string());
                }
            }
        }
        self.last_stop_reason = message
            .and_then(|m| m.get("stop_reason"))
            .and_then(Value::as_str)
            .map(String::from);
        self.tool_result_after_last_assistant = false;
    }

    fn reset_turn(&mut self) {
        self.started = false;
        self.aborted = None;
        self.upstream_error = None;
        self.switch_to = None;
        self.turn_duration = false;
        self.current_message_id = None;
        self.current_texts.clear();
        self.turn_texts.clear();
        self.last_stop_reason = None;
        self.tool_result_after_last_assistant = false;
    }

    fn final_text(&self) -> String {
        if !self.current_texts.is_empty() {
            return self.current_texts.join("\n");
        }
        self.turn_texts
            .iter()
            .rev()
            .find(|text| !text.is_empty())
            .cloned()
            .unwrap_or_default()
    }
}

fn has_block(blocks: &[Value], block_type: &str) -> bool {
    blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some(block_type))
}

/// Text of the first text block that marks a user interrupt, if any.
fn interrupted_text(blocks: &[Value]) -> Option<String> {
    blocks.iter().find_map(|block| {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            return None;
        }
        let text = block.get("text").and_then(Value::as_str)?;
        text.starts_with("[Request interrupted by user")
            .then(|| text.to_string())
    })
}

fn content_blocks(entry: &Value) -> &[Value] {
    entry
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Ordered transcript roots: model env, process env, then `$HOME/.claude*` directories.
pub fn claude_transcript_roots(
    model_env: &BTreeMap<String, String>,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    push_unique(
        &mut roots,
        model_env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
    );
    push_unique(&mut roots, env("CLAUDE_CONFIG_DIR").as_deref());
    if let Some(home) = env("HOME").filter(|value| !value.is_empty()) {
        if let Ok(entries) = std::fs::read_dir(&home) {
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().starts_with(".claude") {
                    continue;
                }
                if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                    push_unique(&mut roots, entry.path().to_str());
                }
            }
        }
    }
    roots
}

fn push_unique(roots: &mut Vec<PathBuf>, path: Option<&str>) {
    let Some(path) = path.filter(|value| !value.is_empty()) else {
        return;
    };
    let path = PathBuf::from(path);
    if !roots.contains(&path) {
        roots.push(path);
    }
}

/// Find `<root>/projects/*/<session_id>.jsonl` under the given roots.
pub fn locate_claude_transcript(roots: &[PathBuf], session_id: &str) -> Option<PathBuf> {
    // The id becomes a file name: allow only uuid-like characters.
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root.join("projects")) else {
            continue;
        };
        for entry in entries.flatten() {
            let candidate = entry.path().join(format!("{session_id}.jsonl"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_fixture(parser: &mut ClaudeTurnParser, content: &str) {
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            let entry: Value = serde_json::from_str(line).expect("fixture line is json");
            parser.feed(&entry);
        }
    }

    fn scan_fixture(content: &str, agent_idle: bool) -> TurnScan {
        let mut parser = ClaudeTurnParser::new();
        feed_fixture(&mut parser, content);
        parser.scan(agent_idle)
    }

    #[test]
    fn simple_end_turn_completes() {
        let scan = scan_fixture(
            include_str!("../../tests/fixtures/claude/simple_end_turn.jsonl"),
            true,
        );
        assert_eq!(
            scan,
            TurnScan::Completed {
                text: "hi there".to_string(),
                stop: StopKind::EndTurn
            }
        );
    }

    #[test]
    fn tool_loop_completes_with_final_text() {
        let scan = scan_fixture(
            include_str!("../../tests/fixtures/claude/tool_loop.jsonl"),
            true,
        );
        assert_eq!(
            scan,
            TurnScan::Completed {
                text: "one file".to_string(),
                stop: StopKind::EndTurn
            }
        );
    }

    #[test]
    fn turn_duration_completes_without_stop_reason_or_idle() {
        let scan = scan_fixture(
            include_str!("../../tests/fixtures/claude/turn_duration_only.jsonl"),
            false,
        );
        assert_eq!(
            scan,
            TurnScan::Completed {
                text: "done".to_string(),
                stop: StopKind::Other("unknown".to_string())
            }
        );
    }

    #[test]
    fn interrupted_turn_is_aborted() {
        let scan = scan_fixture(
            include_str!("../../tests/fixtures/claude/interrupted.jsonl"),
            true,
        );
        assert!(matches!(scan, TurnScan::Aborted { .. }));
    }

    #[test]
    fn interrupted_array_text_block_is_aborted() {
        let scan = scan_fixture(
            include_str!("../../tests/fixtures/claude/interrupted_array.jsonl"),
            true,
        );
        assert_eq!(
            scan,
            TurnScan::Aborted {
                reason: "[Request interrupted by user for tool use]".to_string()
            }
        );
    }

    #[test]
    fn sidechain_meta_and_unknown_entries_are_ignored() {
        let scan = scan_fixture(
            include_str!("../../tests/fixtures/claude/sidechain_ignored.jsonl"),
            true,
        );
        assert_eq!(
            scan,
            TurnScan::Completed {
                text: "real answer".to_string(),
                stop: StopKind::EndTurn
            }
        );
    }

    #[test]
    fn api_error_beats_stop_sequence_completion() {
        let scan = scan_fixture(
            include_str!("../../tests/fixtures/claude/api_error.jsonl"),
            true,
        );
        assert_eq!(
            scan,
            TurnScan::UpstreamError {
                message: "API Error: 529 overloaded".to_string()
            }
        );
    }

    #[test]
    fn continued_in_requests_file_switch() {
        let scan = scan_fixture(
            include_str!("../../tests/fixtures/claude/continued_in.jsonl"),
            true,
        );
        assert_eq!(
            scan,
            TurnScan::SwitchFile {
                session_id: "2f4c1a44-1111-4111-8111-000000000001".to_string()
            }
        );
    }

    #[test]
    fn multi_block_message_joins_text_blocks_of_last_message_id() {
        let scan = scan_fixture(
            include_str!("../../tests/fixtures/claude/multi_block_message.jsonl"),
            true,
        );
        assert_eq!(
            scan,
            TurnScan::Completed {
                text: "part one\npart two".to_string(),
                stop: StopKind::EndTurn
            }
        );
    }

    #[test]
    fn idle_completion_requires_terminal_stop_and_no_trailing_tool_result() {
        let mut parser = ClaudeTurnParser::new();
        let user: Value =
            serde_json::from_str(r#"{"type":"user","message":{"content":"go"}}"#).expect("json");
        let assistant: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#,
        )
        .expect("json");
        parser.feed(&user);
        assert_eq!(parser.scan(true), TurnScan::InProgress);
        parser.feed(&assistant);
        assert_eq!(
            parser.scan(true),
            TurnScan::Completed {
                text: "ok".to_string(),
                stop: StopKind::EndTurn
            }
        );
        assert_eq!(parser.scan(false), TurnScan::InProgress);

        // A tool_result after the final assistant entry blocks idle completion.
        let tool_result: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"x"}]}}"#,
        )
        .expect("json");
        parser.feed(&tool_result);
        assert_eq!(parser.scan(true), TurnScan::InProgress);
    }

    #[test]
    fn tool_result_only_user_entry_does_not_start_a_turn() {
        let mut parser = ClaudeTurnParser::new();
        let tool_result: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"x"}]}}"#,
        )
        .expect("json");
        parser.feed(&tool_result);
        assert_eq!(parser.scan(true), TurnScan::NotStarted);
    }

    #[test]
    fn roots_prefer_model_env_then_process_env_then_home() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-gateway-roots-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".claude")).expect("mkdir .claude");
        std::fs::create_dir_all(home.join(".claude-personal")).expect("mkdir .claude-personal");
        std::fs::create_dir_all(dir.join("model-cfg")).expect("mkdir model-cfg");
        std::fs::write(home.join(".claudenotadir"), "").expect("not a dir");

        let mut model_env = BTreeMap::new();
        model_env.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            dir.join("model-cfg").display().to_string(),
        );
        let roots = claude_transcript_roots(&model_env, |key| match key {
            "CLAUDE_CONFIG_DIR" => Some(dir.join("proc-cfg").display().to_string()),
            "HOME" => Some(home.display().to_string()),
            _ => None,
        });
        assert_eq!(roots[0], dir.join("model-cfg"));
        assert_eq!(roots[1], dir.join("proc-cfg"));
        assert!(roots.contains(&home.join(".claude")));
        assert!(roots.contains(&home.join(".claude-personal")));
        assert!(!roots.contains(&home.join(".claudenotadir")));

        let located = locate_claude_transcript(&roots, "abc123");
        assert_eq!(located, None);
        let project = home.join(".claude").join("projects").join("-tmp");
        std::fs::create_dir_all(&project).expect("mkdir project");
        std::fs::write(project.join("abc123.jsonl"), "{}\n").expect("write transcript");
        let located = locate_claude_transcript(&roots, "abc123");
        assert_eq!(located, Some(project.join("abc123.jsonl")));
        assert_eq!(locate_claude_transcript(&roots, "missing"), None);
        assert_eq!(locate_claude_transcript(&roots, ""), None);
        assert_eq!(locate_claude_transcript(&roots, "a/b"), None);

        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn locate_rejects_path_like_session_ids() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-gateway-claude-ids-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let project = dir.join("projects").join("-tmp");
        std::fs::create_dir_all(&project).expect("mkdir project");
        let uuid = "0b0e6f1c-3c2a-4d5e-9f10-2a3b4c5d6e7f";
        std::fs::write(project.join(format!("{uuid}.jsonl")), "{}\n").expect("write transcript");
        let roots = vec![dir.clone()];

        assert_eq!(
            locate_claude_transcript(&roots, uuid),
            Some(project.join(format!("{uuid}.jsonl")))
        );
        assert_eq!(locate_claude_transcript(&roots, ".."), None);
        assert_eq!(locate_claude_transcript(&roots, "a/b"), None);
        assert_eq!(locate_claude_transcript(&roots, "a\\b"), None);

        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }
}
