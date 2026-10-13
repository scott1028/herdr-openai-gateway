use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{StopKind, TurnScan};

/// Pure incremental turn parser for Codex rollout JSONL entries.
#[derive(Debug, Default)]
pub struct CodexTurnParser {
    started: bool,
    completed: Option<String>,
    aborted: Option<String>,
    upstream_error: Option<String>,
    last_assistant: Option<String>,
    last_final_answer: Option<String>,
    last_agent_message_event: Option<String>,
}

impl CodexTurnParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, entry: &Value) {
        match entry.get("type").and_then(Value::as_str).unwrap_or("") {
            "event_msg" => self.feed_event(entry.get("payload").unwrap_or(&Value::Null)),
            "response_item" => {
                self.feed_response_item(entry.get("payload").unwrap_or(&Value::Null))
            }
            _ => {}
        }
    }

    /// `task_complete` is authoritative, so `agent_idle` is not consulted.
    pub fn scan(&self, _agent_idle: bool) -> TurnScan {
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
        if let Some(text) = &self.completed {
            return TurnScan::Completed {
                text: text.clone(),
                stop: StopKind::EndTurn,
            };
        }
        TurnScan::InProgress
    }

    fn feed_event(&mut self, payload: &Value) {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "task_started" => self.reset_turn(),
            "task_complete" if self.started => {
                if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
                    self.upstream_error = Some(
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown codex error")
                            .to_string(),
                    );
                    return;
                }
                let text =
                    self.resolve_text(payload.get("last_agent_message").and_then(Value::as_str));
                self.completed = Some(text);
            }
            "turn_aborted" if self.started => {
                self.aborted = Some(
                    payload
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("aborted")
                        .to_string(),
                );
            }
            "agent_message" if self.started => {
                if let Some(message) = payload.get("message").and_then(Value::as_str) {
                    self.last_agent_message_event = Some(message.to_string());
                }
            }
            _ => {}
        }
    }

    fn feed_response_item(&mut self, payload: &Value) {
        if !self.started {
            return;
        }
        if payload.get("type").and_then(Value::as_str) != Some("message")
            || payload.get("role").and_then(Value::as_str) != Some("assistant")
        {
            return;
        }
        let text = payload
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if text.is_empty() {
            return;
        }
        self.last_assistant = Some(text.clone());
        if payload.get("phase").and_then(Value::as_str) == Some("final_answer") {
            self.last_final_answer = Some(text);
        }
    }

    fn reset_turn(&mut self) {
        self.started = true;
        self.completed = None;
        self.aborted = None;
        self.upstream_error = None;
        self.last_assistant = None;
        self.last_final_answer = None;
        self.last_agent_message_event = None;
    }

    /// Preference: non-empty `last_agent_message`, then the last `final_answer`
    /// assistant item, then the last assistant item, then the last agent_message event.
    fn resolve_text(&self, last_agent_message: Option<&str>) -> String {
        last_agent_message
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .or_else(|| self.last_final_answer.clone())
            .or_else(|| self.last_assistant.clone())
            .or_else(|| self.last_agent_message_event.clone())
            .unwrap_or_default()
    }
}

/// Codex sessions root: model env `CODEX_HOME`, else the variant's config
/// home, else process `CODEX_HOME`, else `$HOME/.codex`, joined with `sessions`.
pub fn codex_home(
    model_env: &BTreeMap<String, String>,
    variant_home: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> PathBuf {
    model_env
        .get("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| variant_home.map(PathBuf::from))
        .or_else(|| {
            env("CODEX_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from(env("HOME").unwrap_or_default()).join(".codex"))
}

/// The `sessions` directory under [`codex_home`].
pub fn codex_sessions_root(
    model_env: &BTreeMap<String, String>,
    variant_home: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> PathBuf {
    codex_home(model_env, variant_home, env).join("sessions")
}

/// Find `rollout-*-<session_id>.jsonl` under `<sessions_root>/YYYY/MM/DD`,
/// walking date directories newest first.
pub fn locate_codex_rollout(sessions_root: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains('/') {
        return None;
    }
    let suffix = format!("-{session_id}.jsonl");
    for year in sorted_dirs_desc(sessions_root)? {
        for month in sorted_dirs_desc(&year).unwrap_or_default() {
            for day in sorted_dirs_desc(&month).unwrap_or_default() {
                let Ok(entries) = std::fs::read_dir(&day) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("rollout-")
                        && name.ends_with(&suffix)
                        && entry.path().is_file()
                    {
                        return Some(entry.path());
                    }
                }
            }
        }
    }
    None
}

/// Directory entries sorted by name descending so newest dates come first.
fn sorted_dirs_desc(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs.reverse();
    Some(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_fixture(parser: &mut CodexTurnParser, content: &str) {
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            let entry: Value = serde_json::from_str(line).expect("fixture line is json");
            parser.feed(&entry);
        }
    }

    fn scan_fixture(content: &str) -> TurnScan {
        let mut parser = CodexTurnParser::new();
        feed_fixture(&mut parser, content);
        parser.scan(true)
    }

    #[test]
    fn simple_complete_reports_last_agent_message() {
        let scan = scan_fixture(include_str!(
            "../../tests/fixtures/codex/simple_complete.jsonl"
        ));
        assert_eq!(
            scan,
            TurnScan::Completed {
                text: "hi there".to_string(),
                stop: StopKind::EndTurn
            }
        );
    }

    #[test]
    fn empty_last_agent_message_falls_back_to_final_answer() {
        let scan = scan_fixture(include_str!(
            "../../tests/fixtures/codex/final_answer_fallback.jsonl"
        ));
        assert_eq!(
            scan,
            TurnScan::Completed {
                text: "a.txt and b.txt\nare in the folder".to_string(),
                stop: StopKind::EndTurn
            }
        );
    }

    #[test]
    fn turn_aborted_is_aborted() {
        let scan = scan_fixture(include_str!("../../tests/fixtures/codex/aborted.jsonl"));
        assert_eq!(
            scan,
            TurnScan::Aborted {
                reason: "interrupted".to_string()
            }
        );
    }

    #[test]
    fn task_complete_with_error_is_upstream_error() {
        let scan = scan_fixture(include_str!(
            "../../tests/fixtures/codex/error_complete.jsonl"
        ));
        assert_eq!(
            scan,
            TurnScan::UpstreamError {
                message: "Your workspace is out of credits".to_string()
            }
        );
    }

    #[test]
    fn two_turns_report_only_the_second() {
        let scan = scan_fixture(include_str!("../../tests/fixtures/codex/two_turns.jsonl"));
        assert_eq!(
            scan,
            TurnScan::Completed {
                text: "second answer".to_string(),
                stop: StopKind::EndTurn
            }
        );
    }

    #[test]
    fn started_but_unfinished_is_in_progress() {
        let scan = scan_fixture(include_str!("../../tests/fixtures/codex/in_progress.jsonl"));
        assert_eq!(scan, TurnScan::InProgress);
    }

    #[test]
    fn entries_before_task_started_are_not_started() {
        let mut parser = CodexTurnParser::new();
        for line in [
            r#"{"type":"session_meta","payload":{"id":"s"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"early"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"early"}]}}"#,
        ] {
            let entry: Value = serde_json::from_str(line).expect("json");
            parser.feed(&entry);
        }
        assert_eq!(parser.scan(true), TurnScan::NotStarted);
    }

    #[test]
    fn fallback_chain_prefers_final_answer_then_assistant_then_agent_message() {
        let scan_text = |lines: &[&str]| -> String {
            let mut parser = CodexTurnParser::new();
            let start: Value =
                serde_json::from_str(r#"{"type":"event_msg","payload":{"type":"task_started"}}"#)
                    .expect("json");
            parser.feed(&start);
            for line in lines {
                let entry: Value = serde_json::from_str(line).expect("json");
                parser.feed(&entry);
            }
            let complete: Value = serde_json::from_str(
                r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":""}}"#,
            )
            .expect("json");
            parser.feed(&complete);
            match parser.scan(true) {
                TurnScan::Completed { text, .. } => text,
                other => panic!("expected completed, got {other:?}"),
            }
        };
        // No final_answer item: the last assistant item wins.
        assert_eq!(
            scan_text(&[
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"comment"}]}}"#,
            ]),
            "comment"
        );
        // No assistant items: the last agent_message event wins.
        assert_eq!(
            scan_text(&[
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"first","phase":"commentary"}}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"second","phase":"commentary"}}"#,
            ]),
            "second"
        );
        // Nothing at all: empty text.
        assert_eq!(scan_text(&[]), "");
    }

    #[test]
    fn sessions_root_precedence() {
        let mut model_env = BTreeMap::new();
        model_env.insert("CODEX_HOME".to_string(), "/model/codex".to_string());
        let root =
            codex_sessions_root(
                &model_env,
                Some(Path::new("/variant/codex")),
                |key| match key {
                    "CODEX_HOME" => Some("/proc/codex".to_string()),
                    "HOME" => Some("/home/u".to_string()),
                    _ => None,
                },
            );
        assert_eq!(root, PathBuf::from("/model/codex/sessions"));

        let root = codex_sessions_root(
            &BTreeMap::new(),
            Some(Path::new("/variant/codex")),
            |key| match key {
                "CODEX_HOME" => Some("/proc/codex".to_string()),
                "HOME" => Some("/home/u".to_string()),
                _ => None,
            },
        );
        assert_eq!(root, PathBuf::from("/variant/codex/sessions"));

        let root = codex_sessions_root(&BTreeMap::new(), None, |key| match key {
            "CODEX_HOME" => Some("/proc/codex".to_string()),
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        });
        assert_eq!(root, PathBuf::from("/proc/codex/sessions"));

        let root = codex_sessions_root(&BTreeMap::new(), None, |key| match key {
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        });
        assert_eq!(root, PathBuf::from("/home/u/.codex/sessions"));
    }

    #[test]
    fn locate_walks_dates_newest_first_and_matches_suffix() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-openai-server-codex-locate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let root = dir.join("sessions");
        let older = root.join("2025").join("12").join("31");
        let middle = root.join("2026").join("01").join("01");
        let newest = root.join("2026").join("10").join("14");
        std::fs::create_dir_all(&older).expect("mkdir older");
        std::fs::create_dir_all(&middle).expect("mkdir middle");
        std::fs::create_dir_all(&newest).expect("mkdir newest");
        std::fs::write(
            older.join("rollout-2025-12-31T00-00-00-sess-9.jsonl"),
            "{}\n",
        )
        .expect("write older");
        std::fs::write(
            middle.join("rollout-2026-01-01T09-00-00-sess-9.jsonl"),
            "{}\n",
        )
        .expect("write middle");
        // A different session id must not match, and neither must a non-rollout file.
        std::fs::write(
            newest.join("rollout-2026-10-14T00-00-00-other.jsonl"),
            "{}\n",
        )
        .expect("write other");
        std::fs::write(newest.join("history.jsonl"), "{}\n").expect("write history");

        let located = locate_codex_rollout(&root, "sess-9");
        assert_eq!(
            located,
            Some(middle.join("rollout-2026-01-01T09-00-00-sess-9.jsonl")),
            "newest matching date wins"
        );
        std::fs::write(
            newest.join("rollout-2026-10-14T00-00-00-sess-9.jsonl"),
            "{}\n",
        )
        .expect("write newest");
        let located = locate_codex_rollout(&root, "sess-9");
        assert_eq!(
            located,
            Some(newest.join("rollout-2026-10-14T00-00-00-sess-9.jsonl"))
        );

        assert_eq!(locate_codex_rollout(&root, "missing"), None);
        assert_eq!(locate_codex_rollout(&root, ""), None);
        assert_eq!(locate_codex_rollout(&root, "a/b"), None);
        assert_eq!(
            locate_codex_rollout(&dir.join("no-sessions"), "sess-9"),
            None
        );

        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }
}
