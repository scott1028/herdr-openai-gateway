//! Claude PreToolUse question hook payloads and answer conversion.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::dialog::HEADER_MAX_CHARS;

/// Input fields provided to a Claude PreToolUse HTTP hook.
#[derive(Debug, Deserialize)]
pub struct HookInput {
    /// Session identifier used to find the backing Claude agent.
    pub session_id: String,
    /// Name of the Claude tool requesting permission.
    pub tool_name: String,
    /// Claude identifier for this tool invocation.
    pub tool_use_id: String,
    /// Original structured input for the requested tool.
    pub tool_input: Value,
}

/// Parsed answer or dismissal from the client's `question` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAnswer {
    /// Answers keyed by their original question text.
    Answered(BTreeMap<String, String>),
    /// The client dismissed the question or left all questions unanswered.
    Dismissed,
}

/// Convert Claude's question input to the client's `question` tool arguments.
pub fn question_tool_arguments(tool_input: &Value) -> Value {
    let questions = tool_input
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|question| {
            let header: String = question
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(HEADER_MAX_CHARS)
                .collect();
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|option| {
                    json!({
                        "label": option.get("label").cloned().unwrap_or(Value::Null),
                        "description": option.get("description").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "question": question.get("question").cloned().unwrap_or(Value::Null),
                "header": header,
                "options": options,
                "multiple": question.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect::<Vec<_>>();
    json!({"questions": questions})
}

/// Parse the client's response against the ordered question texts.
pub fn parse_answers(content: &str, questions: &[String]) -> HookAnswer {
    if content.contains("The user dismissed this question") {
        return HookAnswer::Dismissed;
    }

    let mut answers = BTreeMap::new();
    let mut cursor = 0;
    for (index, question) in questions.iter().enumerate() {
        let marker = format!("\"{question}\"=\"");
        let Some(start) = content[cursor..]
            .find(&marker)
            .map(|offset| cursor + offset + marker.len())
        else {
            continue;
        };
        let end = if let Some(next_question) = questions.get(index + 1) {
            let next_marker = format!(", \"{next_question}\"=\"");
            content[start..]
                .find(&next_marker)
                .map(|offset| start + offset)
                .or_else(|| {
                    content[start..]
                        .find(". You can now continue")
                        .map(|offset| start + offset)
                })
        } else {
            content[start..]
                .find(". You can now continue")
                .map(|offset| start + offset)
        };
        let Some(end) = end else {
            continue;
        };
        let answer = content[start..end]
            .strip_suffix('"')
            .unwrap_or(&content[start..end]);
        cursor = end;
        if answer != "Unanswered" {
            answers.insert(question.clone(), answer.to_string());
        }
    }

    if answers.is_empty() {
        HookAnswer::Dismissed
    } else {
        HookAnswer::Answered(answers)
    }
}

/// Allow the Claude tool call and add the client's answers to its input.
pub fn allow_response(tool_input: &Value, answers: &BTreeMap<String, String>) -> Value {
    let mut updated_input = tool_input.as_object().cloned().unwrap_or_default();
    updated_input.insert("answers".to_string(), json!(answers));
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": updated_input,
        }
    })
}

/// Deny the Claude tool call with a reason for the client.
pub fn deny_response(reason: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
}

/// Build Claude's HTTP hook settings argument using the token environment var.
pub fn claude_settings_arg(url: &str, timeout_secs: u64) -> String {
    json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "AskUserQuestion",
                "hooks": [{
                    "type": "http",
                    "url": url,
                    "timeout": timeout_secs,
                    "headers": {
                        "X-Herdr-Hook-Token": "$HERDR_OPENAI_GATEWAY_HOOK_TOKEN"
                    },
                    "allowedEnvVars": ["HERDR_OPENAI_GATEWAY_HOOK_TOKEN"],
                }]
            }]
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_input() -> Value {
        json!({
            "questions": [{
                "question": "Which option?",
                "header": "A very long header that should be truncated after thirty chars",
                "options": [
                    {"label": "A", "description": "First choice"},
                    {"label": "B", "description": "Second choice"}
                ],
                "multiSelect": true
            }]
        })
    }

    #[test]
    fn question_tool_arguments_preserve_descriptions_and_truncate_header() {
        let args = question_tool_arguments(&tool_input());
        assert_eq!(
            args["questions"][0]["header"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            30
        );
        assert_eq!(
            args["questions"][0]["options"][0]["description"],
            "First choice"
        );
        assert_eq!(args["questions"][0]["multiple"], true);
    }

    #[test]
    fn parse_answers_keeps_multi_question_values_verbatim() {
        let questions = vec!["Q1".to_string(), "Q2".to_string()];
        let content = "User has answered your questions: \"Q1\"=\"A, B\", \"Q2\"=\"A quote: \\\"yes\\\", and a comma, too\". You can now continue with the user's answers in mind.";
        let HookAnswer::Answered(answers) = parse_answers(content, &questions) else {
            panic!("expected answers");
        };
        assert_eq!(answers["Q1"], "A, B");
        assert_eq!(answers["Q2"], "A quote: \\\"yes\\\", and a comma, too");
    }

    #[test]
    fn parse_answers_handles_single_unanswered_and_dismissed() {
        assert_eq!(
            parse_answers(
                "User has answered your questions: \"Q\"=\"free text\". You can now continue with the user's answers in mind.",
                &["Q".to_string()]
            ),
            HookAnswer::Answered(BTreeMap::from([("Q".to_string(), "free text".to_string())])),
        );
        assert_eq!(
            parse_answers(
                "User has answered your questions: \"Q\"=\"Unanswered\". You can now continue with the user's answers in mind.",
                &["Q".to_string()]
            ),
            HookAnswer::Dismissed,
        );
        assert_eq!(
            parse_answers("The user dismissed this question", &["Q".to_string()]),
            HookAnswer::Dismissed,
        );
        assert_eq!(
            parse_answers(
                "User has answered your questions: \"Q1\"=\"Unanswered\", \"Q2\"=\"Unanswered\". You can now continue with the user's answers in mind.",
                &["Q1".to_string(), "Q2".to_string()]
            ),
            HookAnswer::Dismissed,
        );
    }

    #[test]
    fn allow_response_adds_only_answers_to_updated_input() {
        let input = tool_input();
        let answers = BTreeMap::from([("Which option?".to_string(), "A".to_string())]);
        let response = allow_response(&input, &answers);
        let updated = response["hookSpecificOutput"]["updatedInput"]
            .as_object()
            .unwrap();
        assert_eq!(updated.len(), input.as_object().unwrap().len() + 1);
        for (key, value) in input.as_object().unwrap() {
            assert_eq!(updated[key], *value);
        }
        assert_eq!(updated["answers"]["Which option?"], "A");
    }

    #[test]
    fn claude_settings_arg_uses_the_http_hook_and_env_token() {
        let settings: Value =
            serde_json::from_str(&claude_settings_arg("http://127.0.0.1:8787/hook", 60)).unwrap();
        let hook = &settings["hooks"]["PreToolUse"][0];
        assert_eq!(hook["matcher"], "AskUserQuestion");
        assert_eq!(hook["hooks"][0]["type"], "http");
        assert_eq!(hook["hooks"][0]["url"], "http://127.0.0.1:8787/hook");
        assert_eq!(hook["hooks"][0]["timeout"], 60);
        assert_eq!(
            hook["hooks"][0]["headers"]["X-Herdr-Hook-Token"],
            "$HERDR_OPENAI_GATEWAY_HOOK_TOKEN"
        );
        assert_eq!(
            hook["hooks"][0]["allowedEnvVars"][0],
            "HERDR_OPENAI_GATEWAY_HOOK_TOKEN"
        );
    }
}
