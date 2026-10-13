pub mod bridge;

use crate::openai::{ApiError, NormalizedMessage, NormalizedRequest};

/// Hard cap on a single prompt sent to a backing agent.
pub const MAX_PROMPT_BYTES: usize = 256 * 1024;
/// Budget for flattened bootstrap history.
pub const BOOTSTRAP_MAX_BYTES: usize = 200 * 1024;

/// Strip escape sequences and control characters that could inject TUI control codes.
pub fn sanitize(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .chars()
        .filter(|c| match c {
            '\n' | '\t' => true,
            c if (*c as u32) < 0x20 => false,
            c if *c == '\u{7f}' => false,
            '\u{80}'..='\u{9f}' => false,
            _ => true,
        })
        .collect()
}

pub fn check_prompt_size(prompt: &str) -> Result<(), ApiError> {
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(ApiError::prompt_too_large(format!(
            "prompt exceeds {MAX_PROMPT_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Flatten history for bootstrap: one bracketed block per message, newest kept on overflow.
pub fn flatten_history(messages: &[NormalizedMessage], budget_bytes: usize) -> String {
    let rendered: Vec<String> = messages
        .iter()
        .filter(|message| !matches!(message, NormalizedMessage::System(_)))
        .map(render_message)
        .collect();
    // Walk newest to oldest with a running byte total, then join once.
    let mut kept = 0usize;
    let mut size = 0usize;
    for entry in rendered.iter().rev() {
        let added = if kept == 0 {
            entry.len()
        } else {
            entry.len() + 2
        };
        let omitted = rendered.len() - kept - 1;
        let marker_len = if omitted == 0 {
            0
        } else {
            format!("[{omitted} earlier messages omitted]\n\n").len()
        };
        if size + added + marker_len > budget_bytes {
            break;
        }
        kept += 1;
        size += added;
    }
    let omitted = rendered.len() - kept;
    if omitted == 0 {
        return rendered.join("\n\n");
    }
    let marker = format!("[{omitted} earlier messages omitted]");
    if kept == 0 {
        if marker.len() > budget_bytes {
            return String::new();
        }
        return marker;
    }
    let suffix = rendered[rendered.len() - kept..].join("\n\n");
    format!("{marker}\n\n{suffix}")
}

fn render_message(message: &NormalizedMessage) -> String {
    match message {
        NormalizedMessage::System(_) => String::new(),
        NormalizedMessage::User(text) => format!("[user] {text}"),
        NormalizedMessage::Assistant { text, tool_calls } => {
            let mut out = format!("[assistant] {}", text.clone().unwrap_or_default());
            if !tool_calls.is_empty() {
                let json = serde_json::to_string(tool_calls).unwrap_or_default();
                out.push_str(&format!("\n<tool_calls>{json}</tool_calls>"));
            }
            out
        }
        NormalizedMessage::Tool {
            call_id,
            name,
            content,
        } => match name {
            Some(name) => format!("[tool {call_id} {name}] {content}"),
            None => format!("[tool {call_id}] {content}"),
        },
    }
}

/// First prompt for a self-exec agent; bootstrap replays prior history.
pub fn self_exec_first_prompt(req: &NormalizedRequest, bootstrap: bool) -> String {
    if !bootstrap {
        let trailing: Vec<&str> = req
            .messages
            .iter()
            .rev()
            .take_while(|m| matches!(m, NormalizedMessage::User(_)))
            .map(|m| match m {
                NormalizedMessage::User(text) => text.as_str(),
                _ => "",
            })
            .collect();
        return trailing.into_iter().rev().collect::<Vec<_>>().join("\n\n");
    }
    let split = req
        .messages
        .iter()
        .rposition(|m| matches!(m, NormalizedMessage::User(_)))
        .unwrap_or(req.messages.len());
    let current = match req.messages.get(split) {
        Some(NormalizedMessage::User(text)) => text.clone(),
        _ => String::new(),
    };
    let history = flatten_history(&req.messages[..split], BOOTSTRAP_MAX_BYTES);
    format!("Conversation so far:\n\n{history}\n\nCurrent message:\n\n{current}")
}

/// Next prompt for a self-exec agent: only new user texts; tools belong to the agent.
pub fn self_exec_next_prompt(delta: &[NormalizedMessage]) -> String {
    let mut texts = Vec::new();
    for message in delta {
        match message {
            NormalizedMessage::User(text) => texts.push(text.clone()),
            NormalizedMessage::Tool { call_id, .. } => {
                tracing::warn!(call_id, "dropping tool result in self-exec mode");
            }
            _ => {}
        }
    }
    texts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::{FunctionCall, ToolCall};

    fn req(messages: Vec<NormalizedMessage>) -> NormalizedRequest {
        NormalizedRequest {
            model: "claude".to_string(),
            messages,
            stream: false,
            include_usage: false,
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: true,
            conversation_key: None,
            cwd_header: None,
            client_cwd: None,
            client_peer: None,
            opencode_session: false,
        }
    }

    #[test]
    fn sanitize_strips_escape_injections() {
        let out = sanitize("\x1b[201~pasted\x1b[200~");
        assert!(
            !out.contains('\x1b'),
            "bracketed paste escape must not survive"
        );
        assert!(out.contains("pasted"));
        assert_eq!(sanitize("a\x00b\x07c\x0bd"), "abcd");
        assert_eq!(sanitize("keep\n\ttabs"), "keep\n\ttabs");
        assert_eq!(sanitize("a\u{7f}b"), "ab");
        assert_eq!(sanitize("a\u{80}b\u{9f}c"), "abc");
        assert_eq!(sanitize("crlf\r\nlone cr\r"), "crlf\nlone cr\n");
    }

    #[test]
    fn prompt_size_limit() {
        let ok = "x".repeat(MAX_PROMPT_BYTES);
        assert!(check_prompt_size(&ok).is_ok());
        let too_big = "x".repeat(MAX_PROMPT_BYTES + 1);
        let err = check_prompt_size(&too_big).expect_err("must fail");
        assert_eq!(err.status, 400);
        assert_eq!(err.code, "prompt_too_large");
    }

    #[test]
    fn flatten_history_formats_and_excludes_system() {
        let messages = vec![
            NormalizedMessage::System("ignore me".to_string()),
            NormalizedMessage::User("hello".to_string()),
            NormalizedMessage::Assistant {
                text: Some("calling".to_string()),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "read".to_string(),
                        arguments: "{\"path\":\"a\"}".to_string(),
                    },
                }],
            },
            NormalizedMessage::Tool {
                call_id: "call_1".to_string(),
                name: Some("read".to_string()),
                content: "body".to_string(),
            },
        ];
        let flat = flatten_history(&messages, BOOTSTRAP_MAX_BYTES);
        assert!(!flat.contains("ignore me"));
        assert!(flat.contains("[user] hello"));
        assert!(flat.contains("[assistant] calling\n<tool_calls>"));
        assert!(flat.contains("</tool_calls>"));
        assert!(flat.contains("[tool call_1 read] body"));
        assert!(flat.contains("\n\n"));
    }

    #[test]
    fn flatten_history_keeps_newest_when_over_budget() {
        let messages = vec![
            NormalizedMessage::User("old one".to_string()),
            NormalizedMessage::User("old two".to_string()),
            NormalizedMessage::User("newest".to_string()),
        ];
        let flat = flatten_history(
            &messages,
            "[2 earlier messages omitted]\n\n[user] newest".len(),
        );
        assert_eq!(flat, "[2 earlier messages omitted]\n\n[user] newest");
        let all = flatten_history(&messages, 4096);
        assert!(all.contains("old one"));
    }

    #[test]
    fn first_prompt_without_bootstrap_joins_trailing_user_texts() {
        let r = req(vec![
            NormalizedMessage::System("sys".to_string()),
            NormalizedMessage::User("first".to_string()),
            NormalizedMessage::User("second".to_string()),
        ]);
        assert_eq!(self_exec_first_prompt(&r, false), "first\n\nsecond");
    }

    #[test]
    fn first_prompt_with_bootstrap_splits_current_message() {
        let r = req(vec![
            NormalizedMessage::System("sys".to_string()),
            NormalizedMessage::User("old question".to_string()),
            NormalizedMessage::Assistant {
                text: Some("old answer".to_string()),
                tool_calls: Vec::new(),
            },
            NormalizedMessage::User("current question".to_string()),
        ]);
        let prompt = self_exec_first_prompt(&r, true);
        assert!(prompt.starts_with("Conversation so far:\n\n"));
        assert!(prompt.contains("[user] old question"));
        assert!(prompt.contains("[assistant] old answer"));
        assert!(!prompt.contains("sys"));
        assert!(prompt.ends_with("Current message:\n\ncurrent question"));
        assert!(!prompt.contains("[user] current question"));
    }

    #[test]
    fn next_prompt_joins_user_texts_and_drops_tools() {
        let delta = vec![
            NormalizedMessage::Tool {
                call_id: "call_1".to_string(),
                name: None,
                content: "dropped".to_string(),
            },
            NormalizedMessage::User("a".to_string()),
            NormalizedMessage::User("b".to_string()),
        ];
        assert_eq!(self_exec_next_prompt(&delta), "a\n\nb");
    }
}
