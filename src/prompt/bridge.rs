//! Bridge-mode prompts: protocol preamble, `<tool_results>` rendering, and
//! tool_choice/parallel directives. Pure; the reply parsing lives in `toolcall`.

use sha2::{Digest as _, Sha256};

use crate::openai::{NormalizedMessage, NormalizedRequest};
use crate::prompt::{flatten_history, BOOTSTRAP_MAX_BYTES};

/// Cap on one tool result inside a bridge prompt.
pub const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;

/// What changed between the last prompt sent to the agent and the current request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeChanges {
    pub system_changed: bool,
    pub tools_changed: bool,
}

const PROTOCOL_PREAMBLE: &str = "[herdr-openai-server bridge protocol v1]\nYou are the language model behind another coding tool (\"the client\"). Do not use any tools of your own;\nthe client executes tools for you.";

const PROTOCOL_RULES: &str = "## Output protocol (strict)\nReply with EITHER plain text/markdown, OR optional short text followed by exactly one block:\n\
<tool_calls>\n[{\"name\":\"<tool>\",\"arguments\":{...}}]\n</tool_calls>\n\
and nothing after it. Valid JSON only (double quotes, no comments, no trailing commas, no code fences).\n\
Only use listed tool names. Several independent calls may share one array. After a block, stop: the\n\
results arrive in the next message as <tool_results>. Never invent tool results.";

const REMINDER: &str = "(Reminder: reply with plain text or a single <tool_calls> block.)";

/// All System messages joined with a blank line, verbatim.
pub fn system_text(req: &NormalizedRequest) -> String {
    req.messages
        .iter()
        .filter_map(|message| match message {
            NormalizedMessage::System(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Compact JSON array of `{"name","description","parameters"}` for the request tools.
pub fn tools_json(req: &NormalizedRequest) -> String {
    let described: Vec<serde_json::Value> = req
        .tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.function.name,
                "description": tool.function.description,
                "parameters": tool.function.parameters,
            })
        })
        .collect();
    serde_json::to_string(&described).unwrap_or_else(|_| "[]".to_string())
}

pub fn system_digest(req: &NormalizedRequest) -> String {
    sha256_hex(&system_text(req))
}

pub fn tools_digest(req: &NormalizedRequest) -> String {
    sha256_hex(&tools_json(req))
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One-line directives turning `tool_choice` and `parallel_tool_calls` into instructions.
fn directive_lines(req: &NormalizedRequest) -> Vec<String> {
    let mut lines = Vec::new();
    match &req.tool_choice {
        Some(choice) if choice.as_str() == Some("none") => {
            lines.push("Do not call tools in this reply.".to_string())
        }
        Some(choice) if choice.as_str() == Some("required") => {
            lines.push("You must call at least one tool in this reply.".to_string())
        }
        Some(choice) if choice.get("type").and_then(|v| v.as_str()) == Some("function") => {
            if let Some(name) = choice
                .pointer("/function/name")
                .and_then(|name| name.as_str())
            {
                lines.push(format!("You must call the tool {name} in this reply."));
            }
        }
        _ => {}
    }
    if !req.parallel_tool_calls {
        lines.push("Call at most one tool in this reply.".to_string());
    }
    lines
}

/// First bridge prompt, also used for Fork and respawn.
pub fn bridge_first_prompt(req: &NormalizedRequest, bootstrap: bool) -> String {
    let split = current_split(req);
    let mut sections = vec![PROTOCOL_PREAMBLE.to_string()];
    let system = system_text(req);
    if !system.is_empty() {
        sections.push(format!(
            "## Client instructions\n<client_system>\n{system}\n</client_system>"
        ));
    }
    if !req.tools.is_empty() {
        sections.push(format!(
            "## Tools the client can run\n<tools>\n{}\n</tools>",
            tools_json(req)
        ));
    }
    let mut protocol = PROTOCOL_RULES.to_string();
    for line in directive_lines(req) {
        protocol.push('\n');
        protocol.push_str(&line);
    }
    sections.push(protocol);
    if bootstrap {
        let history = flatten_history(&req.messages[..split], BOOTSTRAP_MAX_BYTES);
        sections.push(format!(
            "## Conversation so far\n<history>\n{history}\n</history>"
        ));
    }
    sections.push(format!(
        "## Current message\n{}",
        render_current(&req.messages[split..])
    ));
    sections.join("\n\n")
}

/// Next bridge prompt: change blocks, tool results, new user texts, and a reminder.
pub fn bridge_next_prompt(
    delta: &[NormalizedMessage],
    changes: &BridgeChanges,
    req: &NormalizedRequest,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if changes.system_changed {
        parts.push(format!(
            "<updated_instructions>\n{}\n</updated_instructions>",
            system_text(req)
        ));
    }
    if changes.tools_changed {
        parts.push(format!(
            "<updated_tools>\n{}\n</updated_tools>",
            tools_json(req)
        ));
    }
    let tools: Vec<(&str, Option<&str>, &str)> = delta
        .iter()
        .filter_map(|message| match message {
            NormalizedMessage::Tool {
                call_id,
                name,
                content,
            } => Some((call_id.as_str(), name.as_deref(), content.as_str())),
            _ => None,
        })
        .collect();
    if !tools.is_empty() {
        parts.push(render_tool_results(&tools));
    }
    let user_texts: Vec<&str> = delta
        .iter()
        .filter_map(|message| match message {
            NormalizedMessage::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if !user_texts.is_empty() {
        parts.push(user_texts.join("\n\n"));
    }
    parts.extend(directive_lines(req));
    parts.push(REMINDER.to_string());
    parts.join("\n\n")
}

/// Wrap tool results for the agent: `<tool_results>` with one `<result>` each.
pub fn render_tool_results(tools: &[(&str, Option<&str>, &str)]) -> String {
    let mut out = String::from("<tool_results>\n");
    for (call_id, name, content) in tools {
        let capped = cap_result(content);
        let escaped = capped.replace("</result>", "<\\/result>");
        let open = match name {
            Some(name) => format!("<result id=\"{call_id}\" name=\"{name}\">"),
            None => format!("<result id=\"{call_id}\">"),
        };
        out.push_str(&open);
        out.push('\n');
        out.push_str(&escaped);
        out.push_str("\n</result>\n");
    }
    out.push_str("</tool_results>");
    out
}

/// Cap content at `MAX_TOOL_RESULT_BYTES` on a char boundary, marking the cut.
fn cap_result(content: &str) -> String {
    if content.len() <= MAX_TOOL_RESULT_BYTES {
        return content.to_string();
    }
    let mut end = MAX_TOOL_RESULT_BYTES;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = content.len() - end;
    format!("{}\n[truncated {omitted} bytes]", &content[..end])
}

/// Index where the current message starts: after the last assistant message, or at
/// the trailing run of user/tool messages when there is none.
fn current_split(req: &NormalizedRequest) -> usize {
    if let Some(index) = req
        .messages
        .iter()
        .rposition(|message| matches!(message, NormalizedMessage::Assistant { .. }))
    {
        return index + 1;
    }
    let mut index = req.messages.len();
    while index > 0 {
        let previous = index - 1;
        if matches!(
            &req.messages[previous],
            NormalizedMessage::User(_) | NormalizedMessage::Tool { .. }
        ) {
            index = previous;
        } else {
            break;
        }
    }
    index
}

/// Render trailing user texts and tool results in message order.
fn render_current(messages: &[NormalizedMessage]) -> String {
    let mut pieces: Vec<String> = Vec::new();
    let mut tools: Vec<(&str, Option<&str>, &str)> = Vec::new();
    for message in messages {
        match message {
            NormalizedMessage::Tool {
                call_id,
                name,
                content,
            } => tools.push((call_id.as_str(), name.as_deref(), content.as_str())),
            NormalizedMessage::User(text) => {
                if !tools.is_empty() {
                    pieces.push(render_tool_results(&tools));
                    tools.clear();
                }
                pieces.push(text.clone());
            }
            _ => {}
        }
    }
    if !tools.is_empty() {
        pieces.push(render_tool_results(&tools));
    }
    pieces.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::{FunctionDef, ToolDef};

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: name.to_string(),
                description: Some("does a thing".to_string()),
                parameters: Some(serde_json::json!({"type": "object"})),
            },
        }
    }

    fn req(messages: Vec<NormalizedMessage>) -> NormalizedRequest {
        NormalizedRequest {
            model: "claude+client-tools".to_string(),
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
    fn first_prompt_has_preamble_system_tools_protocol_and_current_message() {
        let mut r = req(vec![
            NormalizedMessage::System("You are helpful.".to_string()),
            NormalizedMessage::User("read a.txt".to_string()),
        ]);
        r.tools = vec![tool("read")];
        let prompt = bridge_first_prompt(&r, false);
        assert!(prompt.starts_with("[herdr-openai-server bridge protocol v1]"));
        assert!(prompt.contains(
            "## Client instructions\n<client_system>\nYou are helpful.\n</client_system>"
        ));
        assert!(prompt.contains("## Tools the client can run\n<tools>\n["));
        assert!(prompt.contains("\"name\":\"read\""));
        assert!(prompt.contains("\"description\":\"does a thing\""));
        assert!(prompt.contains("\"parameters\":{\"type\":\"object\"}"));
        assert!(prompt.contains("</tools>"));
        assert!(prompt.contains("## Output protocol (strict)"));
        assert!(prompt
            .contains("<tool_calls>\n[{\"name\":\"<tool>\",\"arguments\":{...}}]\n</tool_calls>"));
        assert!(prompt.contains("Never invent tool results."));
        assert!(prompt.ends_with("## Current message\nread a.txt"));
        assert!(!prompt.contains("## Conversation so far"));
    }

    #[test]
    fn first_prompt_omits_missing_sections() {
        let r = req(vec![NormalizedMessage::User("hi".to_string())]);
        let prompt = bridge_first_prompt(&r, false);
        assert!(!prompt.contains("## Client instructions"));
        assert!(!prompt.contains("## Tools the client can run"));
        assert!(prompt.contains("## Output protocol (strict)"));
    }

    #[test]
    fn bootstrap_adds_flattened_history_before_the_current_message() {
        let r = req(vec![
            NormalizedMessage::System("sys".to_string()),
            NormalizedMessage::User("old question".to_string()),
            NormalizedMessage::Assistant {
                text: Some("old answer".to_string()),
                tool_calls: Vec::new(),
            },
            NormalizedMessage::User("now".to_string()),
        ]);
        let prompt = bridge_first_prompt(&r, true);
        assert!(prompt.contains("## Conversation so far\n<history>\n[user] old question"));
        assert!(prompt.contains("[assistant] old answer"));
        assert!(!prompt.contains("<client_system>\nsys\n</client_system>\n\n<tools>"));
        assert!(prompt.ends_with("</history>\n\n## Current message\nnow"));
    }

    #[test]
    fn current_message_renders_trailing_tool_results_in_order() {
        let r = req(vec![
            NormalizedMessage::User("go".to_string()),
            NormalizedMessage::Assistant {
                text: None,
                tool_calls: Vec::new(),
            },
            NormalizedMessage::Tool {
                call_id: "call_1".to_string(),
                name: Some("read".to_string()),
                content: "body".to_string(),
            },
            NormalizedMessage::User("keep going".to_string()),
        ]);
        let prompt = bridge_first_prompt(&r, false);
        assert!(prompt.ends_with(
            "## Current message\n<tool_results>\n<result id=\"call_1\" name=\"read\">\nbody\n</result>\n</tool_results>\n\nkeep going"
        ));
    }

    #[test]
    fn next_prompt_renders_tool_results_and_reminder() {
        let r = req(vec![NormalizedMessage::User("ignored".to_string())]);
        let delta = vec![
            NormalizedMessage::Tool {
                call_id: "call_9".to_string(),
                name: Some("read".to_string()),
                content: "file body".to_string(),
            },
            NormalizedMessage::User("and now?".to_string()),
        ];
        let changes = BridgeChanges {
            system_changed: false,
            tools_changed: false,
        };
        let prompt = bridge_next_prompt(&delta, &changes, &r);
        assert!(prompt.starts_with("<tool_results>\n<result id=\"call_9\" name=\"read\">\nfile body\n</result>\n</tool_results>"));
        assert!(prompt.contains("and now?"));
        assert!(
            prompt.ends_with("(Reminder: reply with plain text or a single <tool_calls> block.)")
        );
        assert!(!prompt.contains("<updated_instructions>"));
        assert!(!prompt.contains("<updated_tools>"));
    }

    #[test]
    fn next_prompt_adds_update_blocks_only_when_changed() {
        let mut r = req(vec![NormalizedMessage::System("new system".to_string())]);
        r.tools = vec![tool("read")];
        let with_changes = bridge_next_prompt(
            &[],
            &BridgeChanges {
                system_changed: true,
                tools_changed: true,
            },
            &r,
        );
        assert!(
            with_changes.starts_with("<updated_instructions>\nnew system\n</updated_instructions>")
        );
        assert!(with_changes.contains("<updated_tools>\n["));
        assert!(with_changes.contains("\"name\":\"read\""));
        let without = bridge_next_prompt(
            &[],
            &BridgeChanges {
                system_changed: false,
                tools_changed: false,
            },
            &r,
        );
        assert!(!without.contains("<updated_instructions>"));
        assert!(!without.contains("<updated_tools>"));
    }

    #[test]
    fn tool_results_escape_and_truncate() {
        let rendered = render_tool_results(&[("call_1", None, "before </result> after")]);
        assert!(rendered.contains("<result id=\"call_1\">"));
        assert!(rendered.contains("before <\\/result> after"));
        assert!(!rendered.contains("before </result> after"));

        let huge = "x".repeat(MAX_TOOL_RESULT_BYTES + 100);
        let rendered = render_tool_results(&[("call_2", Some("t"), &huge)]);
        assert!(rendered.contains(&format!("[truncated {} bytes]", 100)));
        let kept_len = rendered
            .lines()
            .find(|line| line.starts_with('x'))
            .expect("kept line")
            .len();
        assert_eq!(kept_len, MAX_TOOL_RESULT_BYTES);
    }

    #[test]
    fn truncation_lands_on_a_char_boundary() {
        let filler = "a".repeat(MAX_TOOL_RESULT_BYTES - 1);
        let huge = format!("{filler}你好");
        let rendered = render_tool_results(&[("call_3", None, &huge)]);
        assert!(rendered.contains(&filler));
        assert!(rendered.contains("[truncated 6 bytes]"));
    }

    #[test]
    fn directives_map_tool_choice_and_parallel() {
        let base = req(vec![NormalizedMessage::User("q".to_string())]);
        let none = bridge_next_prompt(
            &[],
            &BridgeChanges {
                system_changed: false,
                tools_changed: false,
            },
            &base,
        );
        assert!(!none.contains("Do not call tools"));

        let mut r = base.clone();
        r.tool_choice = Some(serde_json::json!("none"));
        let prompt = bridge_next_prompt(
            &[],
            &BridgeChanges {
                system_changed: false,
                tools_changed: false,
            },
            &r,
        );
        assert!(prompt.contains("Do not call tools in this reply."));

        r.tool_choice = Some(serde_json::json!("required"));
        let prompt = bridge_next_prompt(
            &[],
            &BridgeChanges {
                system_changed: false,
                tools_changed: false,
            },
            &r,
        );
        assert!(prompt.contains("You must call at least one tool in this reply."));

        r.tool_choice = Some(serde_json::json!({"type": "function", "function": {"name": "read"}}));
        let prompt = bridge_next_prompt(
            &[],
            &BridgeChanges {
                system_changed: false,
                tools_changed: false,
            },
            &r,
        );
        assert!(prompt.contains("You must call the tool read in this reply."));

        r.tool_choice = None;
        r.parallel_tool_calls = false;
        let prompt = bridge_next_prompt(
            &[],
            &BridgeChanges {
                system_changed: false,
                tools_changed: false,
            },
            &r,
        );
        assert!(prompt.contains("Call at most one tool in this reply."));
        assert!(
            prompt.ends_with("(Reminder: reply with plain text or a single <tool_calls> block.)")
        );
    }

    #[test]
    fn digests_track_system_text_and_tools_json() {
        let a = req(vec![NormalizedMessage::System("one".to_string())]);
        let mut b = req(vec![NormalizedMessage::System("two".to_string())]);
        assert_ne!(system_digest(&a), system_digest(&b));
        assert_eq!(system_digest(&a), system_digest(&a.clone()));
        assert_eq!(tools_digest(&a), tools_digest(&b));
        b.tools = vec![tool("read")];
        assert_ne!(tools_digest(&a), tools_digest(&b));
    }
}
