//! Parse a bridge agent's reply: plain text plus an optional `<tool_calls>` block
//! carrying strict JSON. Pure.

use serde_json::Value;
use uuid::Uuid;

use crate::openai::{FunctionCall, ToolCall, ToolDef};

const OPEN_TAG: &str = "<tool_calls>";
const CLOSE_TAG: &str = "</tool_calls>";

/// One tool call requested by the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCall {
    pub name: String,
    pub arguments_json: String,
}

/// Parsed agent reply; `error` is Some only when the block exists but is unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReply {
    pub content: Option<String>,
    pub calls: Vec<ParsedCall>,
    pub error: Option<String>,
}

/// Extract calls from the LAST `<tool_calls>` block; an unclosed block runs to
/// the end of the text. Text after the block is dropped.
pub fn parse_reply(text: &str, tools: &[ToolDef], parallel: bool) -> ParsedReply {
    let Some(open) = text.rfind(OPEN_TAG) else {
        let trimmed = text.trim();
        return ParsedReply {
            content: (!trimmed.is_empty()).then(|| trimmed.to_string()),
            calls: Vec::new(),
            error: None,
        };
    };
    let before = text[..open].trim();
    let content = (!before.is_empty()).then(|| before.to_string());
    let after_open = &text[open + OPEN_TAG.len()..];
    let (inner, after) = match after_open.find(CLOSE_TAG) {
        Some(close) => (
            &after_open[..close],
            after_open[close + CLOSE_TAG.len()..].trim(),
        ),
        None => (after_open, ""),
    };
    if !after.is_empty() {
        tracing::debug!(
            dropped = after.len(),
            "dropped text after the <tool_calls> block"
        );
    }
    let fail = |reason: String| ParsedReply {
        content: content.clone(),
        calls: Vec::new(),
        error: Some(reason),
    };

    let body = strip_fences(inner);
    let value = match serde_json::from_str::<Value>(body) {
        Ok(value) => value,
        Err(first) => {
            let repaired = repair_json(body);
            match serde_json::from_str::<Value>(&repaired) {
                Ok(value) => value,
                Err(_) => return fail(format!("invalid JSON in <tool_calls> block: {first}")),
            }
        }
    };
    let items = match value {
        Value::Array(items) => items,
        Value::Object(object) => match object.get("tool_calls") {
            Some(Value::Array(items)) => items.clone(),
            _ => vec![Value::Object(object)],
        },
        _ => return fail("expected a JSON array or object in <tool_calls>".to_string()),
    };
    let mut calls = Vec::with_capacity(items.len());
    for item in items {
        let Some(name) = item_name(&item) else {
            return fail("tool call is missing a name".to_string());
        };
        let Some(name) = resolve_name(name, tools) else {
            return fail(format!("unknown tool: {name}"));
        };
        match item_arguments(&item) {
            Ok(arguments_json) => calls.push(ParsedCall {
                name,
                arguments_json,
            }),
            Err(reason) => return fail(reason),
        }
    }
    if !parallel {
        calls.truncate(1);
    }
    ParsedReply {
        content,
        calls,
        error: None,
    }
}

/// Assign OpenAI call ids (`call_` + 24 lowercase hex) to the parsed calls.
pub fn to_tool_calls(parsed: &ParsedReply) -> Vec<ToolCall> {
    parsed
        .calls
        .iter()
        .map(|call| ToolCall {
            id: new_call_id(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: call.name.clone(),
                arguments: call.arguments_json.clone(),
            },
        })
        .collect()
}

/// OpenAI call id: `call_` + 24 lowercase hex.
pub(crate) fn new_call_id() -> String {
    let hex = Uuid::new_v4().simple().to_string();
    format!("call_{}", &hex[..24])
}

/// Strip a surrounding code fence (```json ... ``` or ``` ... ```).
fn strip_fences(inner: &str) -> &str {
    let trimmed = inner.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    let body = rest.strip_suffix("```").unwrap_or(rest);
    body.trim()
}

/// Minimal JSON repair: smart quotes to ASCII quotes, drop commas before `]`/`}`.
fn repair_json(input: &str) -> String {
    let replaced = input.replace(['\u{201c}', '\u{201d}', '\u{2018}', '\u{2019}'], "\"");
    let chars: Vec<char> = replaced.chars().collect();
    let mut out = String::with_capacity(replaced.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, c) in chars.iter().enumerate() {
        if in_string {
            out.push(*c);
            if escaped {
                escaped = false;
            } else if *c == '\\' {
                escaped = true;
            } else if *c == '"' {
                in_string = false;
            }
            continue;
        }
        if *c == '"' {
            in_string = true;
            out.push(*c);
            continue;
        }
        if *c == ',' {
            let next = chars[index + 1..].iter().find(|c| !c.is_whitespace());
            if matches!(next, Some(']') | Some('}')) {
                continue;
            }
        }
        out.push(*c);
    }
    out
}

fn item_name(item: &Value) -> Option<&str> {
    item.get("name")
        .and_then(Value::as_str)
        .or_else(|| item.get("tool").and_then(Value::as_str))
        .or_else(|| {
            item.get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
        })
}

/// Compact JSON arguments from `arguments`, `parameters`, or `input`; a string
/// value is parsed as JSON first; missing means `{}`.
fn item_arguments(item: &Value) -> Result<String, String> {
    for key in ["arguments", "parameters", "input"] {
        let Some(value) = item.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        return match value {
            Value::String(text) => serde_json::from_str::<Value>(text)
                .map(|parsed| serde_json::to_string(&parsed).unwrap_or_else(|_| "{}".to_string()))
                .map_err(|_| format!("arguments are not valid JSON: {text}")),
            other => Ok(serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string())),
        };
    }
    Ok("{}".to_string())
}

/// Match the call name against request tools: exact first, then case-insensitive
/// with the declared spelling.
fn resolve_name(name: &str, tools: &[ToolDef]) -> Option<String> {
    if let Some(tool) = tools.iter().find(|tool| tool.function.name == name) {
        return Some(tool.function.name.clone());
    }
    let lowered = name.to_lowercase();
    tools
        .iter()
        .find(|tool| tool.function.name.to_lowercase() == lowered)
        .map(|tool| tool.function.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::FunctionDef;

    fn tools(names: &[&str]) -> Vec<ToolDef> {
        names
            .iter()
            .map(|name| ToolDef {
                tool_type: "function".to_string(),
                function: FunctionDef {
                    name: name.to_string(),
                    description: None,
                    parameters: None,
                },
            })
            .collect()
    }

    fn parse(text: &str) -> ParsedReply {
        parse_reply(text, &tools(&["read", "write"]), true)
    }

    #[test]
    fn plain_text_reply_has_no_calls_or_error() {
        let parsed = parse("  just an answer \n");
        assert_eq!(parsed.content.as_deref(), Some("just an answer"));
        assert!(parsed.calls.is_empty());
        assert!(parsed.error.is_none());
        let empty = parse("   ");
        assert_eq!(empty.content, None);
    }

    #[test]
    fn one_call_with_leading_text_kept_as_content() {
        let parsed = parse("thinking...\n<tool_calls>[{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}]</tool_calls>");
        assert_eq!(parsed.content.as_deref(), Some("thinking..."));
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].name, "read");
        assert_eq!(parsed.calls[0].arguments_json, "{\"path\":\"a\"}");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn several_calls_share_one_array() {
        let parsed = parse("<tool_calls>[{\"name\":\"read\",\"arguments\":{}},{\"name\":\"write\",\"arguments\":{\"p\":\"x\"}}]</tool_calls>");
        assert_eq!(parsed.calls.len(), 2);
        assert_eq!(parsed.calls[1].name, "write");
        assert_eq!(parsed.calls[1].arguments_json, "{\"p\":\"x\"}");
    }

    #[test]
    fn parallel_false_keeps_only_the_first_call() {
        let parsed = parse_reply(
            "<tool_calls>[{\"name\":\"write\"},{\"name\":\"read\"}]</tool_calls>",
            &tools(&["read", "write"]),
            false,
        );
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].name, "write");
    }

    #[test]
    fn string_arguments_are_parsed_as_json() {
        let parsed = parse("<tool_calls>[{\"name\":\"read\",\"arguments\":\"{\\\"path\\\": \\\"a\\\"}\"}]</tool_calls>");
        assert_eq!(parsed.calls[0].arguments_json, "{\"path\":\"a\"}");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn missing_arguments_default_to_empty_object() {
        let parsed = parse("<tool_calls>[{\"name\":\"read\"}]</tool_calls>");
        assert_eq!(parsed.calls[0].arguments_json, "{}");
    }

    #[test]
    fn fenced_json_is_accepted() {
        let parsed = parse("<tool_calls>```json\n[{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}]\n```</tool_calls>");
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].arguments_json, "{\"path\":\"a\"}");
        let plain_fence = parse("<tool_calls>```\n[{\"name\":\"read\"}]\n```</tool_calls>");
        assert_eq!(plain_fence.calls.len(), 1);
    }

    #[test]
    fn wrapper_object_and_single_object_are_accepted() {
        let parsed = parse("<tool_calls>{\"tool_calls\":[{\"name\":\"read\",\"arguments\":{\"p\":1}}]}</tool_calls>");
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].arguments_json, "{\"p\":1}");
        let single = parse("<tool_calls>{\"name\":\"read\",\"parameters\":{\"p\":2}}</tool_calls>");
        assert_eq!(single.calls.len(), 1);
        assert_eq!(single.calls[0].arguments_json, "{\"p\":2}");
    }

    #[test]
    fn alternate_field_names_are_accepted() {
        let parsed = parse("<tool_calls>[{\"tool\":\"read\",\"input\":{\"p\":3}},{\"function\":{\"name\":\"write\"},\"arguments\":{\"p\":4}}]</tool_calls>");
        assert_eq!(parsed.calls[0].name, "read");
        assert_eq!(parsed.calls[0].arguments_json, "{\"p\":3}");
        assert_eq!(parsed.calls[1].name, "write");
    }

    #[test]
    fn unclosed_block_at_the_end_is_used() {
        let parsed =
            parse("here:\n<tool_calls>[{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}]");
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.content.as_deref(), Some("here:"));
    }

    #[test]
    fn text_after_the_block_is_dropped() {
        let parsed = parse("<tool_calls>[{\"name\":\"read\"}]</tool_calls>\nand then I did it");
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.content, None);
    }

    #[test]
    fn last_block_wins() {
        let parsed = parse("<tool_calls>[{\"name\":\"write\"}]</tool_calls> oops <tool_calls>[{\"name\":\"read\"}]</tool_calls>");
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].name, "read");
    }

    #[test]
    fn unknown_tool_is_an_error() {
        let parsed = parse("<tool_calls>[{\"name\":\"delete_everything\"}]</tool_calls>");
        assert!(parsed.calls.is_empty());
        let error = parsed.error.expect("error");
        assert!(error.contains("delete_everything"), "got {error}");
    }

    #[test]
    fn case_insensitive_match_uses_the_declared_spelling() {
        let parsed = parse("<tool_calls>[{\"name\":\"Read\"}]</tool_calls>");
        assert_eq!(parsed.calls[0].name, "read");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn trailing_commas_are_repaired() {
        let parsed = parse(
            "<tool_calls>[{\"name\":\"read\",\"arguments\":{\"path\":\"a\",},},]</tool_calls>",
        );
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].arguments_json, "{\"path\":\"a\"}");
    }

    #[test]
    fn smart_quotes_are_repaired() {
        let parsed =
            parse("<tool_calls>[{\u{201c}name\u{201d}:\u{201c}read\u{201d}}]</tool_calls>");
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].name, "read");
    }

    #[test]
    fn invalid_json_is_an_error() {
        let parsed = parse("<tool_calls>this is not json</tool_calls>");
        assert!(parsed.calls.is_empty());
        assert!(parsed.error.expect("error").contains("invalid JSON"));
    }

    #[test]
    fn non_object_array_items_are_errors() {
        let parsed = parse("<tool_calls>[\"read\"]</tool_calls>");
        assert!(parsed.calls.is_empty());
        assert!(parsed.error.expect("error").contains("missing a name"));
    }

    #[test]
    fn call_ids_are_call_plus_24_lowercase_hex() {
        let parsed = parse("<tool_calls>[{\"name\":\"read\"},{\"name\":\"write\"}]</tool_calls>");
        let calls = to_tool_calls(&parsed);
        assert_eq!(calls.len(), 2);
        for call in &calls {
            let hex = call.id.strip_prefix("call_").expect("prefix");
            assert_eq!(hex.len(), 24);
            assert!(hex
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
            assert_eq!(call.call_type, "function");
        }
        assert_ne!(calls[0].id, calls[1].id);
    }
}
