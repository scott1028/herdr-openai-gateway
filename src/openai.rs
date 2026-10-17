use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// OpenAI-shaped error; `body()` renders the wire form without any HTTP framework types.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub error_type: &'static str,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(
        status: u16,
        error_type: &'static str,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            error_type,
            code,
            message: message.into(),
        }
    }

    pub fn body(&self) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "message": self.message,
                "type": self.error_type,
                "param": serde_json::Value::Null,
                "code": self.code,
            }
        })
    }

    fn invalid_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request_error", code, message)
    }
    fn server(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(502, "api_error", code, message)
    }

    pub fn no_new_message(message: impl Into<String>) -> Self {
        Self::invalid_request("no_new_message", message)
    }
    pub fn prompt_too_large(message: impl Into<String>) -> Self {
        Self::invalid_request("prompt_too_large", message)
    }
    pub fn invalid_api_key(message: impl Into<String>) -> Self {
        Self::new(401, "authentication_error", "invalid_api_key", message)
    }
    pub fn cwd_not_allowed(message: impl Into<String>) -> Self {
        Self::new(403, "permission_error", "cwd_not_allowed", message)
    }
    pub fn origin_not_allowed(message: impl Into<String>) -> Self {
        Self::new(403, "permission_error", "origin_not_allowed", message)
    }
    pub fn model_not_found(message: impl Into<String>) -> Self {
        Self::new(404, "not_found_error", "model_not_found", message)
    }
    pub fn unsupported_kind(message: impl Into<String>) -> Self {
        Self::new(404, "not_found_error", "unsupported_kind", message)
    }
    pub fn conversation_busy(message: impl Into<String>) -> Self {
        Self::new(409, "invalid_request_error", "conversation_busy", message)
    }
    pub fn conversation_side_request(message: impl Into<String>) -> Self {
        Self::new(
            409,
            "invalid_request_error",
            "conversation_side_request",
            message,
        )
    }
    pub fn too_many_backing_agents(message: impl Into<String>) -> Self {
        Self::new(429, "rate_limit_error", "too_many_backing_agents", message)
    }
    pub fn backing_agent_exited(message: impl Into<String>) -> Self {
        Self::server("backing_agent_exited", message)
    }
    pub fn agent_start_failed(message: impl Into<String>) -> Self {
        Self::server("agent_start_failed", message)
    }
    pub fn agent_prompt_stalled(message: impl Into<String>) -> Self {
        Self::server("agent_prompt_stalled", message)
    }
    pub fn completion_not_observed(message: impl Into<String>) -> Self {
        Self::server("completion_not_observed", message)
    }
    pub fn herdr_unavailable(message: impl Into<String>) -> Self {
        Self::new(503, "api_error", "herdr_unavailable", message)
    }
    pub fn agent_startup_blocked(message: impl Into<String>) -> Self {
        Self::new(503, "api_error", "agent_startup_blocked", message)
    }
    pub fn turn_timeout(message: impl Into<String>) -> Self {
        Self::new(504, "timeout_error", "turn_timeout", message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{}]", self.message, self.code)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,
    #[serde(default)]
    pub tools: Option<Vec<ToolDef>>,
    #[serde(default)]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
    #[serde(default)]
    pub n: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: Option<MessageContent>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type", default)]
    pub part_type: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type", default)]
    pub tool_type: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default)]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedMessage {
    System(String),
    User(String),
    Assistant {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: Option<String>,
        content: String,
    },
}

#[derive(Debug, Clone)]
pub struct NormalizedRequest {
    pub model: String,
    pub messages: Vec<NormalizedMessage>,
    pub stream: bool,
    pub include_usage: bool,
    pub tools: Vec<ToolDef>,
    pub tool_choice: Option<serde_json::Value>,
    pub parallel_tool_calls: bool,
    pub conversation_key: Option<String>,
    pub cwd_header: Option<String>,
    /// Working directory the client names in its own system prompt, if any.
    pub client_cwd: Option<String>,
    /// TCP peer address supplied by the server connection extractor.
    pub client_peer: Option<SocketAddr>,
    /// Whether the original request carried `x-opencode-session-id`.
    pub opencode_session: bool,
}

pub fn normalize(
    req: ChatRequest,
    header: impl Fn(&str) -> Option<String>,
) -> Result<NormalizedRequest, ApiError> {
    if req.n.unwrap_or(1) > 1 {
        return Err(ApiError::invalid_request(
            "unsupported_parameter",
            "n > 1 is not supported",
        ));
    }
    let mut messages = Vec::with_capacity(req.messages.len());
    for message in req.messages {
        match message.role.as_str() {
            "system" | "developer" => messages.push(NormalizedMessage::System(content_text(
                message.content.as_ref(),
            ))),
            "user" => messages.push(NormalizedMessage::User(content_text(
                message.content.as_ref(),
            ))),
            "assistant" => {
                let text = content_text(message.content.as_ref());
                messages.push(NormalizedMessage::Assistant {
                    text: if text.is_empty() { None } else { Some(text) },
                    tool_calls: message.tool_calls.unwrap_or_default(),
                });
            }
            "tool" | "function" => messages.push(NormalizedMessage::Tool {
                call_id: message.tool_call_id.unwrap_or_default(),
                name: message.name,
                content: content_text(message.content.as_ref()),
            }),
            other => {
                return Err(ApiError::invalid_request(
                    "unsupported_role",
                    format!("unsupported role: {other}"),
                ));
            }
        }
    }
    let conversation_key = [
        "x-herdr-conversation",
        "x-opencode-session-id",
        "x-session-id",
    ]
    .iter()
    .filter_map(|name| header(name))
    .find(|value| !value.is_empty())
    .or_else(|| req.prompt_cache_key.filter(|value| !value.is_empty()))
    .or_else(|| req.user.filter(|value| !value.is_empty()));
    let client_cwd = infer_client_cwd(&messages);
    Ok(NormalizedRequest {
        model: req.model,
        messages,
        stream: req.stream,
        include_usage: req
            .stream_options
            .as_ref()
            .is_some_and(|options| options.include_usage),
        tools: req.tools.unwrap_or_default(),
        tool_choice: req.tool_choice,
        parallel_tool_calls: req.parallel_tool_calls.unwrap_or(true),
        conversation_key,
        cwd_header: header("x-herdr-cwd"),
        client_cwd,
        client_peer: None,
        opencode_session: header("x-opencode-session-id").is_some(),
    })
}

/// opencode writes its working directory into the system prompt, inside an `<env>`
/// block. Take the first `Working directory:` line whose value is an absolute path.
fn infer_client_cwd(messages: &[NormalizedMessage]) -> Option<String> {
    const LINE_PREFIX: &str = "Working directory:";
    for message in messages {
        let NormalizedMessage::System(text) = message else {
            continue;
        };
        for line in text.lines() {
            let value = line
                .trim_start()
                .strip_prefix(LINE_PREFIX)
                .map(|rest| rest.trim())
                .unwrap_or_default();
            if value.starts_with('/') {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Flatten message content: text parts joined with LF, images replaced, unknown parts ignored.
fn content_text(content: Option<&MessageContent>) -> String {
    match content {
        None => String::new(),
        Some(MessageContent::Text(text)) => text.clone(),
        Some(MessageContent::Parts(parts)) => {
            let mut pieces = Vec::new();
            for part in parts {
                match part.part_type.as_str() {
                    "text" => pieces.push(part.text.clone().unwrap_or_default()),
                    "image_url" => pieces.push("[image omitted]".to_string()),
                    _ => {}
                }
            }
            pieces.join("\n")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssistantReply {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletion {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompletionChoice {
    pub index: u32,
    pub message: AssistantMessage,
    pub finish_reason: FinishReason,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssistantMessage {
    pub role: &'static str,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl ChatCompletion {
    pub fn completion(id: &str, model: &str, created: u64, reply: &AssistantReply) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion",
            created,
            model: model.to_string(),
            choices: vec![CompletionChoice {
                index: 0,
                message: AssistantMessage {
                    role: "assistant",
                    content: reply.text.clone(),
                    tool_calls: reply.tool_calls.clone(),
                },
                finish_reason: reply.finish_reason,
            }],
            usage: Usage::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChunkDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallChunk>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallChunk {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub call_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCallChunk>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallChunk {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

impl ChatCompletionChunk {
    pub fn chunk(id: &str, model: &str, created: u64, delta: ChunkDelta) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason: None,
            }],
            usage: None,
        }
    }

    pub fn finish(id: &str, model: &str, created: u64, reason: FinishReason) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason: Some(reason),
            }],
            usage: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelList {
    pub object: &'static str,
    pub data: Vec<ModelObject>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub description: String,
    pub object: &'static str,
    pub created: u32,
    pub owned_by: &'static str,
}

pub fn model_list(models: impl IntoIterator<Item = (String, String)>, created: u32) -> ModelList {
    ModelList {
        object: "list",
        data: models
            .into_iter()
            .map(|(id, description)| ModelObject {
                id,
                description,
                object: "model",
                created,
                owned_by: "herdr-openai-gateway",
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(json: serde_json::Value) -> ChatRequest {
        serde_json::from_value(json).expect("valid request json")
    }

    fn no_headers(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn unknown_request_fields_are_ignored() {
        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.5,
            "top_p": 0.9,
            "seed": 7,
        }));
        let normalized = normalize(req, no_headers).expect("normalizes");
        assert_eq!(normalized.messages.len(), 1);
    }

    #[test]
    fn roles_are_mapped() {
        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [
                {"role": "system", "content": "s1"},
                {"role": "developer", "content": "s2"},
                {"role": "user", "content": "u"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "read", "arguments": "{}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "name": "read", "content": "ok"},
                {"role": "function", "tool_call_id": "call_2", "content": "ok2"},
            ],
        }));
        let normalized = normalize(req, no_headers).expect("normalizes");
        assert_eq!(
            normalized.messages[0],
            NormalizedMessage::System("s1".to_string())
        );
        assert_eq!(
            normalized.messages[1],
            NormalizedMessage::System("s2".to_string())
        );
        assert_eq!(
            normalized.messages[2],
            NormalizedMessage::User("u".to_string())
        );
        match &normalized.messages[3] {
            NormalizedMessage::Assistant { text, tool_calls } => {
                assert_eq!(text, &None);
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].function.name, "read");
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        match &normalized.messages[4] {
            NormalizedMessage::Tool {
                call_id,
                name,
                content,
            } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(name.as_deref(), Some("read"));
                assert_eq!(content, "ok");
            }
            other => panic!("expected tool, got {other:?}"),
        }
        assert!(matches!(
            normalized.messages[5],
            NormalizedMessage::Tool { .. }
        ));
    }

    #[test]
    fn unsupported_role_is_400() {
        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [{"role": "wizard", "content": "hi"}],
        }));
        let err = normalize(req, no_headers).expect_err("must fail");
        assert_eq!(err.status, 400);
        assert_eq!(err.code, "unsupported_role");
    }

    #[test]
    fn content_parts_are_flattened() {
        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "line one"},
                {"type": "image_url", "image_url": {"url": "http://x/y.png"}},
                {"type": "mystery", "data": "ignored"},
                {"type": "text", "text": "line two"},
            ]}],
        }));
        let normalized = normalize(req, no_headers).expect("normalizes");
        assert_eq!(
            normalized.messages[0],
            NormalizedMessage::User("line one\n[image omitted]\nline two".to_string())
        );
    }

    #[test]
    fn n_greater_than_one_is_400() {
        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [],
            "n": 2,
        }));
        let err = normalize(req, no_headers).expect_err("must fail");
        assert_eq!(err.status, 400);
        assert_eq!(err.code, "unsupported_parameter");
    }

    #[test]
    fn conversation_key_precedence() {
        let headers = |name: &str| match name {
            "x-session-id" => Some("from-x-session-id".to_string()),
            _ => None,
        };
        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [],
            "prompt_cache_key": "from-cache-key",
            "user": "from-user",
        }));
        let normalized = normalize(req, headers).expect("normalizes");
        assert_eq!(
            normalized.conversation_key.as_deref(),
            Some("from-x-session-id")
        );

        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [],
            "prompt_cache_key": "from-cache-key",
            "user": "from-user",
        }));
        let normalized = normalize(req, no_headers).expect("normalizes");
        assert_eq!(
            normalized.conversation_key.as_deref(),
            Some("from-cache-key")
        );

        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [],
            "user": "from-user",
        }));
        let normalized = normalize(req, no_headers).expect("normalizes");
        assert_eq!(normalized.conversation_key.as_deref(), Some("from-user"));

        let headers = |name: &str| match name {
            "x-herdr-conversation" => Some("first".to_string()),
            "x-opencode-session-id" => Some("second".to_string()),
            _ => None,
        };
        let req = request(serde_json::json!({"model": "claude", "messages": []}));
        let normalized = normalize(req, headers).expect("normalizes");
        assert_eq!(normalized.conversation_key.as_deref(), Some("first"));
        assert!(normalized.opencode_session);

        let req = request(serde_json::json!({"model": "claude", "messages": []}));
        assert!(
            !normalize(req, no_headers)
                .expect("normalizes")
                .opencode_session
        );
    }

    #[test]
    fn cwd_header_and_defaults() {
        let headers = |name: &str| match name {
            "x-herdr-cwd" => Some("/tmp/project".to_string()),
            _ => None,
        };
        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [],
            "stream": true,
            "stream_options": {"include_usage": true},
        }));
        let normalized = normalize(req, headers).expect("normalizes");
        assert_eq!(normalized.cwd_header.as_deref(), Some("/tmp/project"));
        assert!(normalized.stream);
        assert!(normalized.include_usage);
        assert!(normalized.parallel_tool_calls);

        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [],
            "parallel_tool_calls": false,
        }));
        let normalized = normalize(req, no_headers).expect("normalizes");
        assert!(!normalized.parallel_tool_calls);
        assert!(!normalized.stream);
        assert!(!normalized.include_usage);
    }

    fn env_block(cwd: &str) -> String {
        format!(
            "Here is some useful information about the environment you are running in:\n\
             <env>\n  Working directory: {cwd}\n  Is directory a git repo: yes\n</env>"
        )
    }

    #[test]
    fn client_cwd_comes_from_the_system_prompt_env_block() {
        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [
                {"role": "system", "content": env_block("/home/u/workspace/repo")},
                {"role": "user", "content": "hi"},
            ],
        }));
        let normalized = normalize(req, no_headers).expect("normalizes");
        assert_eq!(
            normalized.client_cwd.as_deref(),
            Some("/home/u/workspace/repo")
        );
        assert_eq!(normalized.cwd_header, None);
    }

    #[test]
    fn client_cwd_reads_developer_messages_and_the_first_match() {
        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "developer", "content": format!("notes\n{}", env_block("/first/pick"))},
                {"role": "system", "content": env_block("/second/pick")},
            ],
        }));
        let normalized = normalize(req, no_headers).expect("normalizes");
        assert_eq!(normalized.client_cwd.as_deref(), Some("/first/pick"));
    }

    #[test]
    fn client_cwd_needs_an_absolute_path_on_its_own_line() {
        for content in [
            "Working directory: ./relative".to_string(),
            "Working directory:".to_string(),
            "prefix Working directory: /not/at/line/start".to_string(),
            "no env block here".to_string(),
        ] {
            let req = request(serde_json::json!({
                "model": "claude",
                "messages": [{"role": "system", "content": content}],
            }));
            let normalized = normalize(req, no_headers).expect("normalizes");
            assert_eq!(normalized.client_cwd, None, "must be ignored");
        }
    }

    #[test]
    fn cwd_header_does_not_feed_client_cwd() {
        let headers = |name: &str| match name {
            "x-herdr-cwd" => Some("/tmp/header".to_string()),
            _ => None,
        };
        let req = request(serde_json::json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let normalized = normalize(req, headers).expect("normalizes");
        assert_eq!(normalized.cwd_header.as_deref(), Some("/tmp/header"));
        assert_eq!(normalized.client_cwd, None);
    }

    #[test]
    fn api_error_body_shape() {
        let err = ApiError::turn_timeout("turn took too long");
        let body = err.body();
        assert_eq!(body["error"]["message"], "turn took too long");
        assert_eq!(body["error"]["type"], "timeout_error");
        assert!(body["error"]["param"].is_null());
        assert_eq!(body["error"]["code"], "turn_timeout");
    }

    #[test]
    fn completion_response_has_zero_usage_and_single_choice() {
        let reply = AssistantReply {
            text: Some("hello".to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
        };
        let completion = ChatCompletion::completion("id-1", "claude", 0, &reply);
        let value = serde_json::to_value(&completion).expect("serializes");
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(value["choices"].as_array().expect("array").len(), 1);
        assert_eq!(value["choices"][0]["message"]["content"], "hello");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        assert_eq!(value["usage"]["total_tokens"], 0);
        assert!(value["choices"][0]["message"].get("tool_calls").is_none());
    }

    #[test]
    fn chunk_and_model_list_shapes() {
        let chunk = ChatCompletionChunk::chunk(
            "id-1",
            "claude",
            0,
            ChunkDelta {
                role: Some("assistant".to_string()),
                ..ChunkDelta::default()
            },
        );
        let value = serde_json::to_value(&chunk).expect("serializes");
        assert_eq!(value["object"], "chat.completion.chunk");
        assert_eq!(value["choices"][0]["delta"]["role"], "assistant");
        assert!(value["choices"][0].get("finish_reason").is_none());

        let list = model_list(
            vec![
                ("claude".to_string(), "Claude tools description".to_string()),
                ("codex".to_string(), "Codex tools description".to_string()),
            ],
            1_700_000_000,
        );
        let value = serde_json::to_value(&list).expect("serializes");
        assert_eq!(value["object"], "list");
        assert_eq!(value["data"][0]["object"], "model");
        assert_eq!(value["data"][0]["owned_by"], "herdr-openai-gateway");
        assert_eq!(value["data"][0]["created"], 1_700_000_000);
        assert_eq!(value["data"][1]["id"], "codex");
        assert_eq!(value["data"][0]["description"], "Claude tools description");
    }
}
